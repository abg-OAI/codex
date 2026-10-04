// Package layercommit translates between accepted Git commits and canonical
// Saffrodex layer definitions. A layer owns a commit message, a patch and
// additive overlay; callers own layer ordering and lifecycle policy.
package layercommit

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"slices"
	"strings"

	"github.com/abg-OAI/codex/layerctl/internal/definition"
	"github.com/abg-OAI/codex/layerctl/internal/gitrepo"
)

// Service captures, applies, and verifies generated layer commits.
type Service struct {
	Git *gitrepo.Repository // required
}

// Captured separates complete added files from the remaining binary-safe delta.
// Together Patch and Overlay reproduce the accepted tree from its predecessor.
type Captured struct {
	Message []byte
	Patch   []byte
	Overlay []definition.OverlayEntry
}

// CaptureRequest selects the predecessor and accepted endpoint for one layer.
type CaptureRequest struct {
	Before string
	After  string
}

// Capture preserves After's commit message and separates its additions from
// inherited changes. Git owns tree comparison and binary patch representation.
func (s *Service) Capture(ctx context.Context, req CaptureRequest) (Captured, error) {
	message, err := commitMessage(s.Git, ctx, s.Git.Root, req.After)
	if err != nil {
		return Captured{}, err
	}
	before, err := s.treeEntries(ctx, req.Before)
	if err != nil {
		return Captured{}, err
	}
	after, err := s.treeEntries(ctx, req.After)
	if err != nil {
		return Captured{}, err
	}
	var overlay []definition.OverlayEntry
	var removals bytes.Buffer
	for _, entry := range after {
		if _, exists := before[entry.Path]; exists || entry.Mode == "160000" {
			continue
		}
		data, err := s.Git.Bytes(ctx, s.Git.Root, "cat-file", "blob", entry.Object)
		if err != nil {
			return Captured{}, err
		}
		addition := definition.OverlayEntry{Path: entry.Path, Mode: entry.Mode, Data: data}
		if err := addition.Validate(); err != nil {
			return Captured{}, err
		}
		overlay = append(overlay, addition)
		fmt.Fprintf(&removals, "0 %040d\t%s%c", 0, entry.Path, 0)
	}
	slices.SortFunc(overlay, func(a, b definition.OverlayEntry) int { return strings.Compare(a.Path, b.Path) })
	residual, err := s.editTree(ctx, req.After, removals.Bytes())
	if err != nil {
		return Captured{}, err
	}
	patch, err := s.diff(ctx, req.Before, residual)
	if err != nil {
		return Captured{}, err
	}
	return Captured{Message: message, Patch: patch, Overlay: overlay}, nil
}

func (s *Service) diff(ctx context.Context, before, after string) ([]byte, error) {
	patch, err := s.Git.Bytes(
		ctx,
		s.Git.Root,
		"-c", "diff.suppressBlankEmpty=true",
		"diff-tree",
		"--patch",
		"--binary",
		"--full-index",
		"--no-commit-id",
		"-r",
		"--no-renames",
		"--no-ext-diff",
		"--no-textconv",
		"--no-color",
		"--diff-algorithm=myers",
		"--src-prefix=a/",
		"--dst-prefix=b/",
		before,
		after,
	)
	if err != nil {
		return nil, fmt.Errorf("diff layer trees: %w", err)
	}
	return patch, nil
}

// Apply applies the patch followed by its disjoint overlay and commits once.
// Git stages clean changes during conflicts; callers retain the worktree for
// resolution. Overlay collisions require refresh, including identical additions.
func (s *Service) Apply(ctx context.Context, worktree string, unit definition.Unit) error {
	patch, err := s.applicationPatch(ctx, unit)
	if err != nil {
		return err
	}
	predecessor, err := s.Git.Output(ctx, worktree, "rev-parse", "HEAD")
	if err != nil {
		return err
	}
	entries, err := s.treeEntries(ctx, predecessor)
	if err != nil {
		return err
	}
	var collisions []string
	for _, entry := range unit.Overlay {
		for path := range entries {
			if path == entry.Path || strings.HasPrefix(path, entry.Path+"/") || strings.HasPrefix(entry.Path, path+"/") {
				collisions = append(collisions, entry.Path)
				break
			}
		}
	}
	if len(patch) != 0 {
		// A single Git application includes the patch followed by disjoint
		// additions. Git stages clean additions even when inherited edits
		// conflict, so continuation cannot omit a pending overlay phase.
		_, err = s.Git.Invoke(ctx, worktree, gitrepo.Invocation{
			Arguments: []string{"-c", "rerere.enabled=false", "apply", "--3way", "--index", "--whitespace=nowarn", "-"},
			Stdin:     patch,
		})
		if err != nil {
			if len(collisions) != 0 {
				return &OverlayCollisionError{Paths: collisions, Cause: err}
			}
			return err
		}
	}
	// Successful application has already removed structural blockers through
	// the patch. Same-leaf additions still need ownership reconciliation.
	var inherited []string
	for _, entry := range unit.Overlay {
		if _, exists := entries[entry.Path]; exists {
			inherited = append(inherited, entry.Path)
		}
	}
	if len(inherited) != 0 {
		return &OverlayCollisionError{Paths: inherited}
	}
	return s.Commit(ctx, worktree, unit)
}

// Commit records the staged tree using unit's exact commit message. It is also
// used after an operator resolves a conflicted three-way application.
func (s *Service) Commit(ctx context.Context, worktree string, unit definition.Unit) error {
	return s.Git.Run(
		ctx,
		worktree,
		"commit",
		"--allow-empty",
		"--no-verify",
		"--no-gpg-sign",
		"--cleanup=verbatim",
		"--file", unit.MessagePath,
	)
}

// HasConflicts reports whether worktree contains unresolved index entries.
func (s *Service) HasConflicts(ctx context.Context, worktree string) (bool, error) {
	output, err := s.Git.Bytes(ctx, worktree, "ls-files", "--unmerged")
	if err != nil {
		return false, fmt.Errorf("inspect unresolved layer paths: %w", err)
	}
	return len(output) != 0, nil
}

// Message reads the exact generated commit message from unit.
func (s *Service) Message(unit definition.Unit) ([]byte, error) {
	message, err := os.ReadFile(unit.MessagePath)
	if err != nil {
		return nil, fmt.Errorf("read layer message %q: %w", unit.MessagePath, err)
	}
	return message, nil
}

// Matches applies unit from before in a disposable worktree and reports
// whether the generated tree and message equal after.
func (s *Service) Matches(ctx context.Context, unit definition.Unit, before, after string) (bool, error) {
	worktree, err := os.MkdirTemp("", "layerctl-layercommit-")
	if err != nil {
		return false, fmt.Errorf("create verification worktree path: %w", err)
	}
	if err := os.Remove(worktree); err != nil {
		return false, fmt.Errorf("prepare verification worktree path %q: %w", worktree, err)
	}
	if err := s.Git.Run(ctx, s.Git.Root, "worktree", "add", "--detach", worktree, before); err != nil {
		return false, fmt.Errorf("create verification worktree: %w", err)
	}
	defer func() {
		_ = s.Git.Run(context.Background(), s.Git.Root, "worktree", "remove", "--force", worktree)
	}()

	if err := s.Apply(ctx, worktree, unit); err != nil {
		return false, fmt.Errorf("apply verification layer: %w", err)
	}
	gotTree, err := s.Git.Output(ctx, worktree, "rev-parse", "HEAD^{tree}")
	if err != nil {
		return false, fmt.Errorf("resolve generated tree: %w", err)
	}
	wantTree, err := s.Git.Output(ctx, s.Git.Root, "rev-parse", after+"^{tree}")
	if err != nil {
		return false, fmt.Errorf("resolve accepted tree: %w", err)
	}
	gotMessage, err := commitMessage(s.Git, ctx, worktree, "HEAD")
	if err != nil {
		return false, err
	}
	wantMessage, err := commitMessage(s.Git, ctx, s.Git.Root, after)
	if err != nil {
		return false, err
	}
	return gotTree == wantTree && bytes.Equal(gotMessage, wantMessage), nil
}

func commitMessage(git *gitrepo.Repository, ctx context.Context, directory, commit string) ([]byte, error) {
	object, err := git.Bytes(ctx, directory, "cat-file", "commit", commit)
	if err != nil {
		return nil, fmt.Errorf("read commit %q: %w", commit, err)
	}
	_, message, ok := bytes.Cut(object, []byte("\n\n"))
	if !ok || len(message) == 0 {
		return nil, fmt.Errorf("commit %q has no message", commit)
	}
	return message, nil
}
