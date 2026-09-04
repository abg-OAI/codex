package layer_test

import (
	"bytes"
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/abg-OAI/codex/layerctl/internal/definition"
	"github.com/abg-OAI/codex/layerctl/internal/gitrepo"
	"github.com/abg-OAI/codex/layerctl/internal/layer"
	"github.com/abg-OAI/codex/layerctl/internal/mailpatch"
	"github.com/abg-OAI/codex/layerctl/internal/projection"
)

func TestAddAndRefreshRoundTripProjectionTrees(t *testing.T) {
	root := newCanonicalRepository(t)
	layers, projections := newServices(t, root)
	addWorktree := filepath.Join(t.TempDir(), "add")

	_, err := projections.Create(t.Context(), projection.CreateRequest{
		Name:         "add-source",
		WorktreePath: addWorktree,
		Through:      "0001-feature",
	})
	if err != nil {
		t.Fatalf("Create(add-source) error = %v", err)
	}
	writeFile(t, filepath.Join(addWorktree, "base.txt"), "added layer\n", 0o644)
	writeFile(t, filepath.Join(addWorktree, "new.sh"), "#!/bin/sh\n", 0o755)
	if err := os.Remove(filepath.Join(addWorktree, "delete.txt")); err != nil {
		t.Fatalf("Remove(delete.txt) error = %v", err)
	}
	gitRun(t, addWorktree, "add", "-A")
	gitRun(t, addWorktree, "commit", "-m", "saffrodex: added layer", "-m", "Exact added body.")
	addSourceHead := gitOutput(t, root, "rev-parse", "refs/layerctl/projections/add-source/head")

	if err := layers.Add(t.Context(), layer.AddRequest{ID: "0002-added", Projection: "add-source"}); err != nil {
		t.Fatalf("Add() error = %v", err)
	}
	patch := readFile(t, filepath.Join(root, "layers", "0002-added.patch"))
	if !strings.Contains(patch, "Subject: saffrodex: added layer") ||
		!strings.Contains(patch, "Exact added body.") ||
		!strings.Contains(patch, "base.txt") ||
		!strings.Contains(patch, "delete.txt") ||
		!strings.Contains(patch, "new.sh") {
		t.Fatalf("captured layer patch is incomplete:\n%s", patch)
	}
	repository, err := definition.Load(root)
	if err != nil {
		t.Fatalf("definition.Load() error = %v", err)
	}
	gotLayerIDs := make([]string, 0, len(repository.Layers))
	for _, unit := range repository.Layers {
		gotLayerIDs = append(gotLayerIDs, unit.ID)
	}
	if want := []string{"0000-foundation", "0001-feature", "0002-added", "0003-tail"}; !slices.Equal(gotLayerIDs, want) {
		t.Fatalf("layer IDs = %v, want %v", gotLayerIDs, want)
	}

	_, projections = newServices(t, root)
	freshAdded, err := projections.Create(t.Context(), projection.CreateRequest{
		Name:    "added-fresh",
		Through: "0002-added",
	})
	if err != nil {
		t.Fatalf("Create(added-fresh) error = %v", err)
	}
	assertSameTree(t, root, addSourceHead, freshAdded.Head)

	layers, projections = newServices(t, root)
	refreshWorktree := filepath.Join(t.TempDir(), "refresh")
	_, err = projections.Create(t.Context(), projection.CreateRequest{
		Name:         "refresh-source",
		WorktreePath: refreshWorktree,
		Through:      "0001-feature",
	})
	if err != nil {
		t.Fatalf("Create(refresh-source) error = %v", err)
	}
	writeFile(t, filepath.Join(refreshWorktree, "base.txt"), "refreshed\n", 0o644)
	writeFile(t, filepath.Join(refreshWorktree, "feature.txt"), "refreshed feature\n", 0o644)
	gitRun(t, refreshWorktree, "add", "-A")
	gitRun(t, refreshWorktree, "commit", "-m", "saffrodex: refreshed feature", "-m", "Exact refresh body.")
	refreshSourceHead := gitOutput(t, root, "rev-parse", "refs/layerctl/projections/refresh-source/head")

	if err := layers.Refresh(t.Context(), "0001-feature", "refresh-source"); err != nil {
		t.Fatalf("Refresh() error = %v", err)
	}
	patch = readFile(t, filepath.Join(root, "layers", "0001-feature.patch"))
	if !strings.Contains(patch, "Subject: saffrodex: refreshed feature") ||
		!strings.Contains(patch, "Exact refresh body.") {
		t.Fatalf("refreshed layer patch has wrong message:\n%s", patch)
	}

	_, projections = newServices(t, root)
	freshRefresh, err := projections.Create(t.Context(), projection.CreateRequest{
		Name:    "refresh-fresh",
		Through: "0001-feature",
	})
	if err != nil {
		t.Fatalf("Create(refresh-fresh) error = %v", err)
	}
	assertSameTree(t, root, refreshSourceHead, freshRefresh.Head)
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
	writeFile(t, filepath.Join(root, "base.txt"), "base\n", 0o644)
	writeFile(t, filepath.Join(root, "delete.txt"), "delete\n", 0o644)
	gitRun(t, root, "add", "-A")
	gitRun(t, root, "commit", "-m", "upstream")
	upstreamCommit := gitOutput(t, root, "rev-parse", "HEAD")
	gitRun(t, root, "tag", "rust-v1.2.3")

	writeFile(t, filepath.Join(root, "foundation.txt"), "foundation\n", 0o644)
	gitRun(t, root, "add", "-A")
	gitRun(t, root, "commit", "-m", "saffrodex: foundation")
	foundationPatch := capturePatch(t, root, "HEAD^", "HEAD")
	writeFile(t, filepath.Join(root, "base.txt"), "feature\n", 0o644)
	writeFile(t, filepath.Join(root, "feature.txt"), "feature\n", 0o644)
	gitRun(t, root, "add", "-A")
	gitRun(t, root, "commit", "-m", "saffrodex: feature")
	featurePatch := capturePatch(t, root, "HEAD^", "HEAD")
	writeFile(t, filepath.Join(root, "tail.txt"), "tail\n", 0o644)
	gitRun(t, root, "add", "-A")
	gitRun(t, root, "commit", "-m", "saffrodex: tail")
	tailPatch := capturePatch(t, root, "HEAD^", "HEAD")
	gitRun(t, root, "switch", "--orphan", "saffrodex-next")

	writeFile(t, filepath.Join(root, "upstream.json"), fmt.Sprintf("{\n  \"tag\": \"rust-v1.2.3\",\n  \"commit\": %q\n}\n", upstreamCommit), 0o644)
	writeFile(t, filepath.Join(root, "layers", "0000-foundation.patch"), string(foundationPatch), 0o644)
	writeFile(t, filepath.Join(root, "layers", "0001-feature.patch"), string(featurePatch), 0o644)
	writeFile(t, filepath.Join(root, "layers", "0003-tail.patch"), string(tailPatch), 0o644)
	gitRun(t, root, "add", "-A")
	gitRun(t, root, "commit", "-m", "canonical")
	return root
}

func newServices(t *testing.T, root string) (*layer.Service, *projection.Service) {
	t.Helper()
	canonical, err := definition.Load(root)
	if err != nil {
		t.Fatalf("definition.Load() error = %v", err)
	}
	git, err := gitrepo.Discover(t.Context(), root)
	if err != nil {
		t.Fatalf("gitrepo.Discover() error = %v", err)
	}
	projections := &projection.Service{
		Definition: canonical,
		Git:        git,
		Log:        log.New(&bytes.Buffer{}, "", 0),
	}
	return &layer.Service{Definition: canonical, Git: git, Projection: projections}, projections
}

func assertSameTree(t *testing.T, root, want, got string) {
	t.Helper()
	command := exec.CommandContext(t.Context(), "git", "diff", "--exit-code", want, got)
	command.Dir = root
	output, err := command.CombinedOutput()
	if err != nil {
		t.Fatalf("trees %s and %s differ: %v\n%s", want, got, err, output)
	}
}

func writeFile(t *testing.T, path, content string, mode os.FileMode) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatalf("MkdirAll(%q) error = %v", filepath.Dir(path), err)
	}
	if err := os.WriteFile(path, []byte(content), mode); err != nil {
		t.Fatalf("WriteFile(%q) error = %v", path, err)
	}
}

func readFile(t *testing.T, path string) string {
	t.Helper()
	content, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("ReadFile(%q) error = %v", path, err)
	}
	return string(content)
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

func capturePatch(t *testing.T, directory, before, after string) []byte {
	t.Helper()
	git, err := gitrepo.Discover(t.Context(), directory)
	if err != nil {
		t.Fatalf("gitrepo.Discover() error = %v", err)
	}
	patches := &mailpatch.Service{Git: git}
	content, err := patches.Capture(t.Context(), mailpatch.CaptureRequest{
		Before: before,
		After:  after,
	})
	if err != nil {
		t.Fatalf("Capture(%s, %s) error = %v", before, after, err)
	}
	return content
}
