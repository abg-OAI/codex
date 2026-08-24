package projection_test

import (
	"bytes"
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/abg-OAI/codex/layerctl/internal/definition"
	"github.com/abg-OAI/codex/layerctl/internal/gitrepo"
	"github.com/abg-OAI/codex/layerctl/internal/projection"
)

func TestProjectionLifecycleUsesCustomRefs(t *testing.T) {
	repositoryRoot := newCanonicalRepository(t)
	service := newService(t, repositoryRoot)
	worktreePath := filepath.Join(t.TempDir(), "projection")

	created, err := service.Create(t.Context(), projection.CreateRequest{
		Name:         "feature-edit",
		WorktreePath: worktreePath,
	})
	if err != nil {
		t.Fatalf("Create() error = %v", err)
	}
	if created.WorktreePath != worktreePath {
		t.Fatalf("Create().WorktreePath = %q, want %q", created.WorktreePath, worktreePath)
	}
	if created.Base != created.Head {
		t.Fatalf("Create() base = %q, head = %q", created.Base, created.Head)
	}
	if got := gitOutput(t, repositoryRoot, "show", "-s", "--format=%B", created.Head); got != "saffrodex: feature\n\nFeature body." {
		t.Fatalf("generated feature commit message = %q", got)
	}
	if got := gitOutput(t, repositoryRoot, "show", "-s", "--format=%B", created.Head+"^"); got != "saffrodex: foundation\n\nFoundation body." {
		t.Fatalf("generated foundation commit message = %q", got)
	}

	assertFile(t, filepath.Join(worktreePath, "base.txt"), "changed\n")
	assertFile(t, filepath.Join(worktreePath, "foundation.txt"), "foundation\n")
	assertFile(t, filepath.Join(worktreePath, "feature.txt"), "feature\n")
	if got := gitOutput(t, worktreePath, "symbolic-ref", "HEAD"); got != "refs/layerctl/projections/feature-edit/head" {
		t.Fatalf("symbolic HEAD = %q", got)
	}
	if got := gitOutput(t, repositoryRoot, "for-each-ref", "--format=%(refname)", "refs/heads"); strings.Contains(got, "feature-edit") {
		t.Fatalf("refs/heads contains projection: %q", got)
	}

	writeFile(t, filepath.Join(worktreePath, "work.txt"), "one\n")
	gitRun(t, worktreePath, "add", "work.txt")
	gitRun(t, worktreePath, "commit", "-m", "work one")
	firstHead := gitOutput(t, repositoryRoot, "rev-parse", "refs/layerctl/projections/feature-edit/head")
	if firstHead == created.Head {
		t.Fatal("ordinary commit did not advance custom head ref")
	}

	writeFile(t, filepath.Join(worktreePath, "work.txt"), "amended\n")
	gitRun(t, worktreePath, "add", "work.txt")
	gitRun(t, worktreePath, "commit", "--amend", "--no-edit")
	amendedHead := gitOutput(t, repositoryRoot, "rev-parse", "refs/layerctl/projections/feature-edit/head")
	if amendedHead == firstHead {
		t.Fatal("commit --amend did not advance custom head ref")
	}

	writeFile(t, filepath.Join(worktreePath, "second.txt"), "second\n")
	gitRun(t, worktreePath, "add", "second.txt")
	gitRun(t, worktreePath, "commit", "-m", "work two")
	beforeRebase := gitOutput(t, repositoryRoot, "rev-parse", "refs/layerctl/projections/feature-edit/head")
	gitRunWithEnv(
		t,
		worktreePath,
		[]string{"GIT_COMMITTER_DATE=2030-01-01T00:00:00Z"},
		"rebase",
		"--force-rebase",
		"refs/layerctl/projections/feature-edit/base",
	)
	afterRebase := gitOutput(t, repositoryRoot, "rev-parse", "refs/layerctl/projections/feature-edit/head")
	if afterRebase == beforeRebase {
		t.Fatal("rebase did not advance custom head ref")
	}

	listed, err := service.List(t.Context())
	if err != nil {
		t.Fatalf("List() error = %v", err)
	}
	if len(listed) != 1 || listed[0].Name != "feature-edit" || listed[0].WorktreePath != worktreePath {
		t.Fatalf("List() = %#v", listed)
	}
	path, err := service.Path(t.Context(), "feature-edit")
	if err != nil {
		t.Fatalf("Path() error = %v", err)
	}
	if path != worktreePath {
		t.Fatalf("Path() = %q, want %q", path, worktreePath)
	}

	writeFile(t, filepath.Join(worktreePath, "dirty.txt"), "discard me\n")
	if err := service.Delete(t.Context(), "feature-edit"); err != nil {
		t.Fatalf("Delete() error = %v", err)
	}
	if _, err := os.Stat(worktreePath); !os.IsNotExist(err) {
		t.Fatalf("deleted worktree Stat() error = %v, want not exist", err)
	}
	if got := gitOutput(t, repositoryRoot, "for-each-ref", "--format=%(refname)", "refs/layerctl/projections/feature-edit"); got != "" {
		t.Fatalf("projection refs after Delete() = %q", got)
	}
}

func TestCreateThroughFoundationLayerOmitsLaterLayers(t *testing.T) {
	repositoryRoot := newCanonicalRepository(t)
	service := newService(t, repositoryRoot)

	created, err := service.Create(t.Context(), projection.CreateRequest{
		Name:    "foundation-only",
		Through: "0000-foundation",
	})
	if err != nil {
		t.Fatalf("Create() error = %v", err)
	}
	if got := gitOutput(t, repositoryRoot, "show", created.Head+":foundation.txt"); got != "foundation" {
		t.Fatalf("foundation content = %q", got)
	}
	command := exec.CommandContext(t.Context(), "git", "show", created.Head+":feature.txt")
	command.Dir = repositoryRoot
	if err := command.Run(); err == nil {
		t.Fatal("feature.txt exists in foundation-only projection")
	}
}

func newCanonicalRepository(t *testing.T) string {
	t.Helper()
	root := filepath.Join(t.TempDir(), "repository")
	if err := os.MkdirAll(root, 0o755); err != nil {
		t.Fatalf("MkdirAll(%q) error = %v", root, err)
	}
	gitRun(t, root, "init", "--initial-branch=upstream")
	gitRun(t, root, "config", "user.name", "Layerctl Test")
	gitRun(t, root, "config", "user.email", "layerctl@example.com")
	writeFile(t, filepath.Join(root, "AGENTS.md"), "upstream agents\n")
	writeFile(t, filepath.Join(root, "README.md"), "upstream readme\n")
	writeFile(t, filepath.Join(root, "base.txt"), "base\n")
	gitRun(t, root, "add", "-A")
	gitRun(t, root, "commit", "-m", "upstream")
	upstreamCommit := gitOutput(t, root, "rev-parse", "HEAD")
	gitRun(t, root, "tag", "rust-v1.2.3")

	gitRun(t, root, "switch", "--orphan", "saffrodex-next")
	writeFile(t, filepath.Join(root, "upstream.json"), fmt.Sprintf("{\n  \"tag\": \"rust-v1.2.3\",\n  \"commit\": %q\n}\n", upstreamCommit))
	writeFile(t, filepath.Join(root, "layers", "0000-foundation", "COMMIT_MSG"), "saffrodex: foundation\n\nFoundation body.\n")
	writeFile(t, filepath.Join(root, "layers", "0000-foundation", "overlay", "foundation.txt"), "foundation\n")
	writeFile(t, filepath.Join(root, "layers", "0001-feature", "COMMIT_MSG"), "saffrodex: feature\n\nFeature body.\n")
	writeFile(t, filepath.Join(root, "layers", "0001-feature", "overlay", "feature.txt"), "feature\n")
	writeFile(t, filepath.Join(root, "layers", "0001-feature", "patches", "001-base.patch"), "diff --git a/base.txt b/base.txt\n--- a/base.txt\n+++ b/base.txt\n@@ -1 +1 @@\n-base\n+changed\n")
	gitRun(t, root, "add", "-A")
	gitRun(t, root, "commit", "-m", "canonical")
	return root
}

func newService(t *testing.T, root string) *projection.Service {
	t.Helper()
	canonical, err := definition.Load(root)
	if err != nil {
		t.Fatalf("definition.Load() error = %v", err)
	}
	git, err := gitrepo.Discover(t.Context(), root)
	if err != nil {
		t.Fatalf("gitrepo.Discover() error = %v", err)
	}
	return &projection.Service{
		Definition: canonical,
		Git:        git,
		Log:        log.New(&bytes.Buffer{}, "", 0),
	}
}

func gitRun(t *testing.T, directory string, args ...string) {
	t.Helper()
	command := exec.CommandContext(t.Context(), "git", args...)
	command.Dir = directory
	output, err := command.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v error = %v\n%s", args, err, output)
	}
}

func gitRunWithEnv(t *testing.T, directory string, env []string, args ...string) {
	t.Helper()
	command := exec.CommandContext(t.Context(), "git", args...)
	command.Dir = directory
	command.Env = append(os.Environ(), env...)
	output, err := command.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v error = %v\n%s", args, err, output)
	}
}

func gitOutput(t *testing.T, directory string, args ...string) string {
	t.Helper()
	command := exec.CommandContext(t.Context(), "git", args...)
	command.Dir = directory
	output, err := command.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v error = %v\n%s", args, err, output)
	}
	return strings.TrimSpace(string(output))
}

func writeFile(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatalf("MkdirAll(%q) error = %v", filepath.Dir(path), err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatalf("WriteFile(%q) error = %v", path, err)
	}
}

func assertFile(t *testing.T, path, want string) {
	t.Helper()
	content, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("ReadFile(%q) error = %v", path, err)
	}
	if got := string(content); got != want {
		t.Fatalf("ReadFile(%q) = %q, want %q", path, got, want)
	}
}
