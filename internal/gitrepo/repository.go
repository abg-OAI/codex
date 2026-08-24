// Package gitrepo provides the bounded system-Git operations needed by
// layerctl. It owns command construction and error reporting, not Saffrodex
// projection policy.
package gitrepo

import (
	"bytes"
	"context"
	"fmt"
	"os/exec"
	"strings"
)

// Repository runs Git commands against one shared repository.
type Repository struct {
	Root string
}

// Discover resolves the repository containing directory.
func Discover(ctx context.Context, directory string) (*Repository, error) {
	root, err := output(ctx, directory, "rev-parse", "--show-toplevel")
	if err != nil {
		return nil, err
	}
	return &Repository{Root: root}, nil
}

// Output runs Git in directory and returns trimmed standard output.
func (r *Repository) Output(ctx context.Context, directory string, args ...string) (string, error) {
	output, err := r.Bytes(ctx, directory, args...)
	return strings.TrimSpace(string(output)), err
}

// Run runs Git in directory and discards standard output after successful
// completion. Failures retain both output streams in the returned error.
func (r *Repository) Run(ctx context.Context, directory string, args ...string) error {
	_, err := r.Bytes(ctx, directory, args...)
	return err
}

// Bytes runs Git in directory without altering standard output bytes.
func (r *Repository) Bytes(ctx context.Context, directory string, args ...string) ([]byte, error) {
	return commandOutput(ctx, directory, args...)
}

func output(ctx context.Context, directory string, args ...string) (string, error) {
	output, err := commandOutput(ctx, directory, args...)
	return strings.TrimSpace(string(output)), err
}

func commandOutput(ctx context.Context, directory string, args ...string) ([]byte, error) {
	command := exec.CommandContext(ctx, "git", args...)
	command.Dir = directory
	var stdout bytes.Buffer
	var stderr bytes.Buffer
	command.Stdout = &stdout
	command.Stderr = &stderr
	if err := command.Run(); err != nil {
		detail := strings.TrimSpace(stderr.String())
		if detail == "" {
			detail = strings.TrimSpace(stdout.String())
		}
		if detail == "" {
			return nil, fmt.Errorf("git %s: %w", strings.Join(args, " "), err)
		}
		return nil, fmt.Errorf("git %s: %w: %s", strings.Join(args, " "), err, detail)
	}
	return stdout.Bytes(), nil
}
