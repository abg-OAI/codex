package mailpatch_test

import (
	"bytes"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/abg-OAI/codex/layerctl/internal/gitrepo"
	"github.com/abg-OAI/codex/layerctl/internal/mailpatch"
)

func TestService_CaptureAndApplyRoundTripAcceptedEndpoint(t *testing.T) {
	root, before := newRepository(t)
	writeFile(t, filepath.Join(root, "text.txt"), []byte("intermediate\n"), 0o644)
	writeFile(t, filepath.Join(root, "binary.bin"), []byte{0, 1, 2, 255}, 0o644)
	gitRun(t, root, "add", "-A")
	gitRun(t, root, "commit", "-m", "intermediate edit")

	writeFile(t, filepath.Join(root, "text.txt"), []byte("accepted\n"), 0o644)
	writeFile(t, filepath.Join(root, "script.sh"), []byte("#!/bin/sh\n"), 0o755)
	if err := os.Remove(filepath.Join(root, "deleted.txt")); err != nil {
		t.Fatalf("Remove(deleted.txt) error = %v", err)
	}
	if err := os.Remove(filepath.Join(root, "replace")); err != nil {
		t.Fatalf("Remove(replace) error = %v", err)
	}
	writeFile(t, filepath.Join(root, "replace", "child.txt"), []byte("directory\n"), 0o644)
	if err := os.Symlink("text.txt", filepath.Join(root, "link")); err != nil {
		t.Fatalf("Symlink(link) error = %v", err)
	}
	gitRun(t, root, "add", "-A")
	gitRun(
		t,
		root,
		"-c", "user.name=Accepted Author",
		"-c", "user.email=accepted@example.com",
		"commit",
		"-m", "layer: Capture endpoint",
		"-m", "Preserve the complete accepted tree.",
	)
	after := gitOutput(t, root, "rev-parse", "HEAD")

	git, err := gitrepo.Discover(t.Context(), root)
	if err != nil {
		t.Fatalf("gitrepo.Discover() error = %v", err)
	}
	patches := &mailpatch.Service{Git: git}
	content, err := patches.Capture(t.Context(), mailpatch.CaptureRequest{
		Before: before,
		After:  after,
	})
	if err != nil {
		t.Fatalf("Capture() error = %v", err)
	}
	patchPath := filepath.Join(t.TempDir(), "0001-feature.patch")
	writeFile(t, patchPath, content, 0o644)
	if !bytes.Contains(content, []byte("GIT binary patch")) {
		t.Fatal("Capture() patch does not contain the binary delta")
	}

	gitRun(t, root, "config", "apply.whitespace", "fix")
	matches, err := patches.Matches(t.Context(), patchPath, before, after)
	if err != nil {
		t.Fatalf("Matches() error = %v", err)
	}
	if !matches {
		t.Fatal("Matches() = false, want true")
	}
}

func TestService_CaptureRejectsMailSeparatorInMessage(t *testing.T) {
	root, before := newRepository(t)
	writeFile(t, filepath.Join(root, "text.txt"), []byte("changed\n"), 0o644)
	gitRun(t, root, "add", "-A")
	gitRun(t, root, "commit", "-m", "layer: Invalid message", "-m", "Before\n\n---\n\nAfter")

	git, err := gitrepo.Discover(t.Context(), root)
	if err != nil {
		t.Fatalf("gitrepo.Discover() error = %v", err)
	}
	patches := &mailpatch.Service{Git: git}
	_, err = patches.Capture(t.Context(), mailpatch.CaptureRequest{
		Before: before,
		After:  "HEAD",
	})
	if err == nil || !strings.Contains(err.Error(), "standalone --- line") {
		t.Fatalf("Capture() error = %v, want standalone separator error", err)
	}
}

func TestService_ValidateRejectsSeveralMessages(t *testing.T) {
	root, before := newRepository(t)
	git, err := gitrepo.Discover(t.Context(), root)
	if err != nil {
		t.Fatalf("gitrepo.Discover() error = %v", err)
	}
	patches := &mailpatch.Service{Git: git}

	writeFile(t, filepath.Join(root, "one.txt"), []byte("one\n"), 0o644)
	gitRun(t, root, "add", "-A")
	gitRun(t, root, "commit", "-m", "layer: One")
	one, err := patches.Capture(t.Context(), mailpatch.CaptureRequest{
		Before: before,
		After:  "HEAD",
	})
	if err != nil {
		t.Fatalf("Capture(one) error = %v", err)
	}
	oneCommit := gitOutput(t, root, "rev-parse", "HEAD")
	writeFile(t, filepath.Join(root, "two.txt"), []byte("two\n"), 0o644)
	gitRun(t, root, "add", "-A")
	gitRun(t, root, "commit", "-m", "layer: Two")
	two, err := patches.Capture(t.Context(), mailpatch.CaptureRequest{
		Before: oneCommit,
		After:  "HEAD",
	})
	if err != nil {
		t.Fatalf("Capture(two) error = %v", err)
	}

	patchPath := filepath.Join(t.TempDir(), "0001-several.patch")
	writeFile(t, patchPath, append(one, two...), 0o644)
	if err := patches.Validate(t.Context(), patchPath); err == nil ||
		!strings.Contains(err.Error(), "contains 2 messages") {
		t.Fatalf("Validate() error = %v, want several-message error", err)
	}
}

func newRepository(t *testing.T) (string, string) {
	t.Helper()
	root := t.TempDir()
	gitRun(t, root, "init", "--initial-branch=main")
	gitRun(t, root, "config", "user.name", "Layerctl Test")
	gitRun(t, root, "config", "user.email", "layerctl@example.com")
	writeFile(t, filepath.Join(root, "text.txt"), []byte("base\n"), 0o644)
	writeFile(t, filepath.Join(root, "deleted.txt"), []byte("delete\n"), 0o644)
	writeFile(t, filepath.Join(root, "replace"), []byte("file\n"), 0o644)
	gitRun(t, root, "add", "-A")
	gitRun(t, root, "commit", "-m", "base")
	return root, gitOutput(t, root, "rev-parse", "HEAD")
}

func writeFile(t *testing.T, path string, content []byte, mode os.FileMode) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatalf("MkdirAll(%q) error = %v", filepath.Dir(path), err)
	}
	if err := os.WriteFile(path, content, mode); err != nil {
		t.Fatalf("WriteFile(%q) error = %v", path, err)
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
