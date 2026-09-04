// Package mailpatch serializes one Saffrodex layer as one Git mail patch.
// It owns the Git mail format and the transient git-am operation; callers own
// layer ordering, projection lifecycle, and upstream advancement policy.
package mailpatch

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/abg-OAI/codex/layerctl/internal/gitrepo"
)

// Service translates accepted commits to canonical patches and applies them.
type Service struct {
	Git *gitrepo.Repository // required
}

// Capture returns one canonical mail patch whose parent is Before and whose
// tree, message, author, and author date come from After.
func (s *Service) Capture(ctx context.Context, req CaptureRequest) ([]byte, error) {
	beforeTree, err := s.Git.Output(ctx, s.Git.Root, "rev-parse", req.Before+"^{tree}")
	if err != nil {
		return nil, fmt.Errorf("resolve predecessor tree: %w", err)
	}
	afterTree, err := s.Git.Output(ctx, s.Git.Root, "rev-parse", req.After+"^{tree}")
	if err != nil {
		return nil, fmt.Errorf("resolve accepted tree: %w", err)
	}
	if beforeTree == afterTree {
		return nil, errors.New("accepted commit has no tree changes")
	}

	message, err := commitMessage(s.Git, ctx, s.Git.Root, req.After)
	if err != nil {
		return nil, err
	}
	if err := validateMessage(message); err != nil {
		return nil, err
	}
	author, err := commitAuthor(s.Git, ctx, s.Git.Root, req.After)
	if err != nil {
		return nil, err
	}

	synthetic, err := s.Git.Invoke(ctx, s.Git.Root, gitrepo.Invocation{
		Arguments: []string{"commit-tree", afterTree, "-p", req.Before},
		Stdin:     message,
		Environment: []string{
			"GIT_AUTHOR_NAME=" + author.Name,
			"GIT_AUTHOR_EMAIL=" + author.Email,
			"GIT_AUTHOR_DATE=" + author.Date,
			"GIT_COMMITTER_NAME=" + author.Name,
			"GIT_COMMITTER_EMAIL=" + author.Email,
			"GIT_COMMITTER_DATE=" + author.Date,
		},
	})
	if err != nil {
		return nil, fmt.Errorf("construct layer commit: %w", err)
	}

	patch, err := s.Git.Bytes(
		ctx,
		s.Git.Root,
		"-c", "diff.suppressBlankEmpty=true",
		"format-patch",
		"--stdout",
		"--binary",
		"--full-index",
		"--zero-commit",
		"--no-signature",
		"--no-stat",
		"--keep-subject",
		"--no-numbered",
		"--no-renames",
		"--no-ext-diff",
		"--no-textconv",
		"--no-color",
		"--diff-algorithm=myers",
		"--src-prefix=a/",
		"--dst-prefix=b/",
		"-1",
		strings.TrimSpace(string(synthetic)),
	)
	if err != nil {
		return nil, fmt.Errorf("format layer patch: %w", err)
	}
	return patch, nil
}

// CaptureRequest selects the predecessor and accepted endpoint for one layer.
type CaptureRequest struct {
	Before string
	After  string
}

// Apply validates and commits exactly one layer patch in worktree. When the
// three-way fallback conflicts, Git leaves an operation for Continue or Abort.
func (s *Service) Apply(ctx context.Context, worktree, patchPath string) error {
	if err := s.Validate(ctx, patchPath); err != nil {
		return err
	}
	if err := s.Git.Run(
		ctx,
		worktree,
		"-c", "rerere.enabled=false",
		"am",
		"--3way",
		"--no-verify",
		"--keep",
		"--keep-cr",
		"--no-scissors",
		"--quoted-cr=nowarn",
		"--whitespace=nowarn",
		"--no-rerere-autoupdate",
		"--no-gpg-sign",
		"--resolvemsg=Resolve conflicts, stage the desired tree, then run layerctl upstream continue.",
		"--",
		patchPath,
	); err != nil {
		return err
	}
	return nil
}

// Continue commits the resolution for the active layer patch.
func (s *Service) Continue(ctx context.Context, worktree string) error {
	return s.Git.Run(
		ctx,
		worktree,
		"-c", "rerere.enabled=false",
		"am",
		"--continue",
		"--no-verify",
		"--no-rerere-autoupdate",
		"--no-gpg-sign",
	)
}

// Abort restores the commit that preceded the active layer patch.
func (s *Service) Abort(ctx context.Context, worktree string) error {
	return s.Git.Run(ctx, worktree, "am", "--abort")
}

// InProgress reports whether worktree contains an active git-am operation.
func (s *Service) InProgress(ctx context.Context, worktree string) (bool, error) {
	gitPath, err := s.Git.Output(ctx, worktree, "rev-parse", "--git-path", "rebase-apply")
	if err != nil {
		return false, err
	}
	if !filepath.IsAbs(gitPath) {
		gitPath = filepath.Join(worktree, gitPath)
	}
	_, err = os.Stat(filepath.Join(gitPath, "applying"))
	if err == nil {
		return true, nil
	}
	if errors.Is(err, os.ErrNotExist) {
		return false, nil
	}
	return false, fmt.Errorf("inspect git-am state: %w", err)
}

// Validate rejects layer files that Git would interpret as zero or several
// messages, or whose commit body contains the mail-patch separator.
func (s *Service) Validate(ctx context.Context, patchPath string) error {
	content, err := os.ReadFile(patchPath)
	if err != nil {
		return fmt.Errorf("read layer patch %q: %w", patchPath, err)
	}
	if err := validateMessage(content); err != nil {
		return fmt.Errorf("validate layer patch %q: %w", patchPath, err)
	}

	directory, err := os.MkdirTemp("", "layerctl-mailsplit-")
	if err != nil {
		return fmt.Errorf("create mail validation directory: %w", err)
	}
	defer os.RemoveAll(directory)
	count, err := s.Git.Output(ctx, s.Git.Root, "mailsplit", "--keep-cr", "-o"+directory, "--", patchPath)
	if err != nil {
		return fmt.Errorf("parse layer patch %q: %w", patchPath, err)
	}
	if count != "1" {
		return fmt.Errorf("layer patch %q contains %s messages; exactly one is required", patchPath, count)
	}
	return nil
}

// Message returns the commit message that Apply will create from patchPath.
func (s *Service) Message(ctx context.Context, patchPath string) ([]byte, error) {
	if err := s.Validate(ctx, patchPath); err != nil {
		return nil, err
	}
	content, err := os.ReadFile(patchPath)
	if err != nil {
		return nil, fmt.Errorf("read layer patch %q: %w", patchPath, err)
	}
	directory, err := os.MkdirTemp("", "layerctl-mailinfo-")
	if err != nil {
		return nil, fmt.Errorf("create mail inspection directory: %w", err)
	}
	defer os.RemoveAll(directory)
	messagePath := filepath.Join(directory, "message")
	diffPath := filepath.Join(directory, "diff")
	metadata, err := s.Git.Invoke(ctx, s.Git.Root, gitrepo.Invocation{
		Arguments: []string{
			"mailinfo",
			"-k",
			"--no-scissors",
			"--encoding=UTF-8",
			messagePath,
			diffPath,
		},
		Stdin: content,
	})
	if err != nil {
		return nil, fmt.Errorf("inspect layer patch %q: %w", patchPath, err)
	}
	subject := mailinfoField(metadata, "Subject")
	if len(subject) == 0 {
		return nil, fmt.Errorf("layer patch %q has no subject", patchPath)
	}
	body, err := os.ReadFile(messagePath)
	if err != nil {
		return nil, fmt.Errorf("read message from layer patch %q: %w", patchPath, err)
	}
	body = bytes.TrimSuffix(body, []byte("\n"))
	message := append(append([]byte{}, subject...), '\n')
	if len(body) > 0 {
		message = append(message, '\n')
		message = append(message, body...)
	}
	return message, nil
}

// Matches applies patchPath from before in a disposable worktree and reports
// whether the resulting tree, message, author, and author date equal after.
func (s *Service) Matches(ctx context.Context, patchPath, before, after string) (bool, error) {
	worktree, err := os.MkdirTemp("", "layerctl-mailpatch-")
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

	if err := s.Apply(ctx, worktree, patchPath); err != nil {
		return false, fmt.Errorf("apply verification patch: %w", err)
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
	gotAuthor, err := commitAuthor(s.Git, ctx, worktree, "HEAD")
	if err != nil {
		return false, err
	}
	wantAuthor, err := commitAuthor(s.Git, ctx, s.Git.Root, after)
	if err != nil {
		return false, err
	}
	return gotTree == wantTree &&
		bytes.Equal(gotMessage, wantMessage) &&
		gotAuthor == wantAuthor, nil
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

func commitAuthor(git *gitrepo.Repository, ctx context.Context, directory, commit string) (author, error) {
	output, err := git.Bytes(ctx, directory, "show", "-s", "--format=%an%x00%ae%x00%aI%x00", commit)
	if err != nil {
		return author{}, fmt.Errorf("read author for commit %q: %w", commit, err)
	}
	fields := bytes.Split(bytes.TrimSuffix(output, []byte("\n")), []byte{0})
	if len(fields) != 4 || len(fields[3]) != 0 {
		return author{}, fmt.Errorf("cannot parse author for commit %q", commit)
	}
	return author{Name: string(fields[0]), Email: string(fields[1]), Date: string(fields[2])}, nil
}

type author struct {
	Name  string
	Email string
	Date  string
}

func validateMessage(content []byte) error {
	for _, line := range bytes.Split(content, []byte("\n")) {
		if bytes.Equal(line, []byte("---")) {
			return errors.New("commit messages cannot contain a standalone --- line")
		}
	}
	return nil
}

func mailinfoField(metadata []byte, name string) []byte {
	prefix := []byte(name + ": ")
	for _, line := range bytes.Split(metadata, []byte("\n")) {
		if bytes.HasPrefix(line, prefix) {
			return bytes.TrimPrefix(line, prefix)
		}
	}
	return nil
}
