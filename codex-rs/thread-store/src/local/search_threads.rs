use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use codex_rollout::RolloutConfig;
use codex_rollout::RolloutSnippetMatcher;
use codex_rollout::parse_cursor;
use futures::FutureExt;
use futures::future::pending;
use tokio::sync::mpsc;

use super::LocalThreadStore;
use super::helpers::resolve_thread_names;
use super::helpers::resolve_thread_section_metadata;
use super::helpers::set_thread_name;
use super::helpers::stored_thread_from_rollout_item;
use super::list_threads::list_rollout_threads;
use crate::ListThreadsParams;
use crate::SearchThreadsParams;
use crate::SortDirection;
use crate::StoredThreadSearchResult;
use crate::ThreadSearchObserver;
use crate::ThreadSearchPage;
use crate::ThreadSortKey;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

#[cfg(test)]
#[path = "search_threads_tests.rs"]
mod tests;

#[derive(Clone)]
struct ThreadSearchItem {
    item: codex_rollout::ThreadItem,
    snippet: String,
}

pub(super) async fn search_threads(
    store: &LocalThreadStore,
    params: SearchThreadsParams,
) -> ThreadStoreResult<ThreadSearchPage> {
    search_threads_observed(
        store,
        params,
        ThreadSearchObserver {
            progress: None,
            cancelled: Box::pin(pending()),
        },
    )
    .await
}

pub(super) async fn search_threads_observed(
    store: &LocalThreadStore,
    params: SearchThreadsParams,
    observer: ThreadSearchObserver,
) -> ThreadStoreResult<ThreadSearchPage> {
    codex_rollout::with_thread_search_read_limit(search_catalog(store, params, observer))
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: err.to_string(),
        })?
}

async fn search_catalog(
    store: &LocalThreadStore,
    params: SearchThreadsParams,
    observer: ThreadSearchObserver,
) -> ThreadStoreResult<ThreadSearchPage> {
    let cancelled = observer.cancelled.shared();
    let search_term = params.search_term.as_str();
    if params.page_size == 0 {
        return Err(ThreadStoreError::InvalidRequest {
            message: "thread/search requires a positive page size".into(),
        });
    }
    if search_term.is_empty() {
        return Err(ThreadStoreError::InvalidRequest {
            message: "thread/search requires search_term".to_string(),
        });
    }
    let cursor = params
        .cursor
        .as_deref()
        .map(|cursor| {
            parse_cursor(cursor).ok_or_else(|| ThreadStoreError::InvalidRequest {
                message: format!("invalid cursor: {cursor}"),
            })
        })
        .transpose()?;
    let sort_key = match params.sort_key {
        ThreadSortKey::CreatedAt => codex_rollout::ThreadSortKey::CreatedAt,
        ThreadSortKey::UpdatedAt => codex_rollout::ThreadSortKey::UpdatedAt,
        ThreadSortKey::RecencyAt => codex_rollout::ThreadSortKey::RecencyAt,
        ThreadSortKey::SectionPosition => {
            return Err(ThreadStoreError::InvalidRequest {
                message: "section-position sorting requires a section filter".to_owned(),
            });
        }
    };
    let sort_direction = match params.sort_direction {
        SortDirection::Asc => codex_rollout::SortDirection::Asc,
        SortDirection::Desc => codex_rollout::SortDirection::Desc,
    };
    let state_db = tokio::select! {
        biased;
        () = cancelled.clone() => return Err(search_cancelled()),
        db = store.state_db() => db,
    };
    let rollout_config = RolloutConfig {
        codex_home: store.config.codex_home.clone(),
        sqlite: store.config.sqlite.clone(),
        cwd: store.config.codex_home.clone(),
        model_provider_id: store.config.default_model_provider_id.clone(),
        generate_memories: false,
    };
    let mut matcher = RolloutSnippetMatcher::new(search_term);
    if observer.progress.is_some() {
        matcher
            .compile()
            .map_err(|err| ThreadStoreError::InvalidRequest {
                message: format!("invalid rollout search term: {err}"),
            })?;
    }
    tokio::select! {
        biased;
        () = cancelled => Err(search_cancelled()),
        result = finish_search(
            SearchContext { store, params: &params, state_db, rollout_config: &rollout_config, cursor,
                sort_key, sort_direction, matcher: &mut matcher }, observer.progress,
        ) => result,
    }
}

fn search_cancelled() -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: "thread search cancelled".to_string(),
    }
}

struct SearchContext<'a, 'term> {
    store: &'a LocalThreadStore,
    params: &'a SearchThreadsParams,
    state_db: Option<codex_rollout::StateDbHandle>,
    rollout_config: &'a RolloutConfig,
    cursor: Option<codex_rollout::Cursor>,
    sort_key: codex_rollout::ThreadSortKey,
    sort_direction: codex_rollout::SortDirection,
    matcher: &'a mut RolloutSnippetMatcher<'term>,
}

async fn finish_search(
    context: SearchContext<'_, '_>,
    progress: Option<mpsc::Sender<Vec<StoredThreadSearchResult>>>,
) -> ThreadStoreResult<ThreadSearchPage> {
    let SearchContext {
        store,
        params,
        state_db,
        rollout_config,
        cursor,
        sort_key,
        sort_direction,
        matcher,
    } = context;
    let mut matching_items = Vec::new();
    let mut page_cursor = cursor;
    let scan_page_size = params.page_size.saturating_mul(8).clamp(256, 2048);
    let scan_params = ListThreadsParams {
        included_thread_ids: None,
        page_size: scan_page_size,
        cursor: None,
        sort_key: params.sort_key,
        sort_direction: params.sort_direction,
        allowed_sources: params.allowed_sources.clone(),
        model_providers: None,
        cwd_filters: None,
        section: None,
        project_id: None,
        archived: params.archived,
        search_term: None,
        relation_filter: params
            .include_attached_threads
            .then_some(crate::ThreadRelationFilter::IncludeAttached),
        use_state_db_only: state_db.is_some(),
    };
    let root = store.config.codex_home.join(if params.archived {
        codex_rollout::ARCHIVED_SESSIONS_SUBDIR
    } else {
        codex_rollout::SESSIONS_SUBDIR
    });
    let root = match tokio::fs::canonicalize(root).await {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ThreadSearchPage {
                items: Vec::new(),
                next_cursor: None,
            });
        }
        Err(error) => {
            return Err(ThreadStoreError::Internal {
                message: error.to_string(),
            });
        }
    };
    loop {
        let page = list_rollout_threads(
            state_db.clone(),
            rollout_config,
            store.config.default_model_provider_id.as_str(),
            &scan_params,
            page_cursor.as_ref(),
            sort_key,
            sort_direction,
            codex_rollout::state_db::RolloutPathCheck::WhenOpening,
        )
        .await?;
        let page = codex_rollout::ThreadsPage::from(page);
        let page_thread_ids: Vec<_> = page
            .items
            .iter()
            .filter_map(|item| item.thread_id)
            .collect();
        let mut original_paths = None;
        for item in page.items {
            if !matches_source(params, &item) {
                continue;
            }
            // A shared catalog can contain another Codex home's rollout paths.
            let Some(path) = selected_search_path(&root, &item).await? else {
                continue;
            };
            let Some(id) = item.thread_id else { continue };
            let mut reader = match codex_rollout::open_rollout_line_reader(&path).await {
                Ok(reader) => reader,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(search_read_error(error)),
            };
            let header = codex_rollout::read_session_meta_from_reader(&path, &mut reader)
                .await
                .map_err(search_read_error)?;
            if header.meta.id != id {
                continue;
            }
            if let Some(account) = &params.account
                && !account
                    .includes_header(
                        &store.config.codex_home,
                        id,
                        &path,
                        header.meta,
                        &page_thread_ids,
                        &mut original_paths,
                    )
                    .await
            {
                continue;
            }
            let snippet = matcher
                .first_match_in(reader)
                .await
                .map_err(search_read_error)?;
            let Some(snippet) = snippet else { continue };
            let hit = ThreadSearchItem { item, snippet };
            if let Some(progress) = &progress
                && matching_items.len() < params.page_size.min(8)
            {
                let data = materialize_search_results(
                    store,
                    state_db.as_deref(),
                    params.archived,
                    vec![hit.clone()],
                )
                .await;
                if !data.is_empty() {
                    let _ = progress.try_send(data);
                }
            }
            matching_items.push(hit);
            if matching_items.len() > params.page_size {
                break;
            }
        }
        page_cursor = page.next_cursor;
        if matching_items.len() > params.page_size || page_cursor.is_none() {
            break;
        }
    }

    let more_matches_available = matching_items.len() > params.page_size;
    matching_items.truncate(params.page_size);
    let next_cursor = if more_matches_available {
        matching_items.last().and_then(|item| {
            if scan_params.use_state_db_only {
                cursor_from_thread_search_item(item, params.sort_key)
            } else {
                item.item.filesystem_cursor(sort_key)
            }
        })
    } else {
        None
    }
    .as_ref()
    .and_then(|cursor| serde_json::to_value(cursor).ok())
    .and_then(|value| value.as_str().map(str::to_owned));

    let items =
        materialize_search_results(store, state_db.as_deref(), params.archived, matching_items)
            .await;
    Ok(ThreadSearchPage { items, next_cursor })
}

fn search_read_error(err: std::io::Error) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!("failed to read rollout search match: {err}"),
    }
}

fn matches_source(params: &SearchThreadsParams, item: &codex_rollout::ThreadItem) -> bool {
    params.source_kinds.as_ref().is_none_or(|kinds| {
        kinds.iter().any(|kind| {
            kind.matches(
                item.source
                    .as_ref()
                    .unwrap_or(&codex_protocol::protocol::SessionSource::Unknown),
            )
        })
    })
}

async fn materialize_search_results(
    store: &LocalThreadStore,
    state_db: Option<&codex_state::StateRuntime>,
    archived: bool,
    matching_items: Vec<ThreadSearchItem>,
) -> Vec<StoredThreadSearchResult> {
    let mut items = matching_items
        .into_iter()
        .filter_map(|item| {
            stored_thread_from_rollout_item(
                item.item,
                archived,
                store.config.default_model_provider_id.as_str(),
            )
            .map(|thread| StoredThreadSearchResult {
                thread,
                snippet: item.snippet,
            })
        })
        .collect::<Vec<_>>();
    if let Some(state_db) = state_db {
        let sectioned_thread_ids = items
            .iter()
            .filter(|item| item.thread.section.is_some())
            .map(|item| item.thread.thread_id)
            .collect::<Vec<_>>();
        let mut section_metadata =
            resolve_thread_section_metadata(state_db, &sectioned_thread_ids).await;
        for item in &mut items {
            if let Some((section_position, section_entered_at)) =
                section_metadata.remove(&item.thread.thread_id)
            {
                item.thread.section_position = section_position;
                item.thread.section_entered_at = section_entered_at;
            }
        }
    }
    set_thread_search_result_names(store, &mut items).await;

    items
}

async fn selected_search_path(
    root: &Path,
    item: &codex_rollout::ThreadItem,
) -> ThreadStoreResult<Option<PathBuf>> {
    if item.thread_id.is_none()
        || codex_rollout::thread_id_from_rollout_path(&item.path) != item.thread_id
    {
        return Ok(None);
    }
    let Some(path) = codex_rollout::existing_rollout_path(&item.path).await else {
        return Ok(None);
    };
    let canonical = match tokio::fs::canonicalize(&path).await {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ThreadStoreError::Internal {
                message: error.to_string(),
            });
        }
    };
    if !canonical.starts_with(root)
        || !tokio::fs::symlink_metadata(&path)
            .await
            .is_ok_and(|info| info.is_file())
    {
        return Ok(None);
    }
    Ok(Some(canonical))
}

fn cursor_from_thread_search_item(
    item: &ThreadSearchItem,
    sort_key: ThreadSortKey,
) -> Option<codex_rollout::Cursor> {
    let timestamp = match sort_key {
        ThreadSortKey::CreatedAt => item.item.created_at.as_deref()?,
        ThreadSortKey::UpdatedAt => item
            .item
            .updated_at
            .as_deref()
            .or(item.item.created_at.as_deref())?,
        ThreadSortKey::RecencyAt => item
            .item
            .recency_at
            .as_deref()
            .or(item.item.updated_at.as_deref())
            .or(item.item.created_at.as_deref())?,
        ThreadSortKey::SectionPosition => return None,
    };
    parse_cursor(&format!("{timestamp}|{}", item.item.thread_id?))
}

async fn set_thread_search_result_names(
    store: &LocalThreadStore,
    items: &mut [StoredThreadSearchResult],
) {
    let thread_history_modes = items
        .iter()
        .map(|item| (item.thread.thread_id, item.thread.history_mode))
        .collect::<HashMap<_, _>>();
    let names = resolve_thread_names(store, &thread_history_modes).await;
    for item in items {
        if let Some(name) = names.get(&item.thread.thread_id).cloned() {
            set_thread_name(&mut item.thread, name);
        }
    }
}
