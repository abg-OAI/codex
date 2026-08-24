// Package layer captures projection trees into canonical Saffrodex layer
// definitions. It owns classification of new paths and inherited-path patches.
package layer

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"

	"github.com/abg-OAI/codex/layerctl/internal/definition"
	"github.com/abg-OAI/codex/layerctl/internal/gitrepo"
	"github.com/abg-OAI/codex/layerctl/internal/projection"
)

// Service owns updates to the canonical layers directory.
type Service struct {
	Definition *definition.Repository // required
	Git        *gitrepo.Repository    // required
	Projection *projection.Service    // required
}

// AddRequest describes a new numbered layer captured from a projection.
type AddRequest struct {
	ID         string
	Projection string
}

// Add captures a projection's editable range as a new canonical layer.
func (s *Service) Add(ctx context.Context, req AddRequest) error {
	if err := definition.ValidateLayerID(req.ID); err != nil {
		return err
	}
	if slices.ContainsFunc(s.Definition.Layers, func(unit definition.Unit) bool { return unit.ID == req.ID }) {
		return fmt.Errorf("layer %q already exists", req.ID)
	}
	position, _ := slices.BinarySearchFunc(s.Definition.Layers, req.ID, func(unit definition.Unit, id string) int {
		return strings.Compare(unit.ID, id)
	})
	if position == 0 {
		return fmt.Errorf("new layer %q must sort after %q", req.ID, definition.FoundationLayerID)
	}
	source, err := s.Projection.Get(ctx, req.Projection)
	if err != nil {
		return err
	}
	if source.Base == source.Head {
		return fmt.Errorf("projection %q has no changes to capture", req.Projection)
	}
	predecessor := s.Definition.Layers[position-1]
	if err := s.requireGeneratedUnit(ctx, source.Base, predecessor); err != nil {
		return fmt.Errorf("projection %q base: %w", req.Projection, err)
	}
	return s.writeLayer(ctx, req.ID, source.Base, source.Head, nil)
}

// Refresh replaces an existing layer so its generated result becomes the
// source projection head. The projection base must be that layer's generated
// commit, making its first parent the comparison tree.
func (s *Service) Refresh(ctx context.Context, id, projectionName string) error {
	if err := definition.ValidateLayerID(id); err != nil {
		return err
	}
	current := s.findUnit(id)
	if current == nil {
		return fmt.Errorf("layer %q does not exist", id)
	}
	source, err := s.Projection.Get(ctx, projectionName)
	if err != nil {
		return err
	}
	baseMatches, err := s.isGeneratedUnit(ctx, source.Base, *current)
	if err != nil {
		return err
	}
	before := source.Base + "^"
	if !baseMatches {
		headMatches, err := s.isGeneratedUnit(ctx, source.Head, *current)
		if err != nil {
			return err
		}
		if !headMatches {
			return fmt.Errorf("projection %q does not end at generated layer %q", projectionName, id)
		}
		before = source.Head + "^"
	}
	parent, err := s.Git.Output(ctx, s.Git.Root, "rev-parse", before)
	if err != nil {
		return fmt.Errorf("resolve parent of generated layer %q: %w", id, err)
	}
	return s.writeLayer(ctx, id, parent, source.Head, current)
}

func (s *Service) findUnit(id string) *definition.Unit {
	for i := range s.Definition.Layers {
		if s.Definition.Layers[i].ID == id {
			return &s.Definition.Layers[i]
		}
	}
	return nil
}

func (s *Service) requireGeneratedUnit(ctx context.Context, commit string, unit definition.Unit) error {
	matches, err := s.isGeneratedUnit(ctx, commit, unit)
	if err != nil {
		return err
	}
	if !matches {
		return fmt.Errorf("generated commit is not unit %q", unit.ID)
	}
	return nil
}

func (s *Service) isGeneratedUnit(ctx context.Context, commit string, unit definition.Unit) (bool, error) {
	message, err := commitMessage(s.Git, ctx, s.Git.Root, commit)
	if err != nil {
		return false, err
	}
	wantMessage, err := os.ReadFile(unit.CommitMessagePath)
	if err != nil {
		return false, fmt.Errorf("read commit message for %q: %w", unit.ID, err)
	}
	return bytes.Equal(message, wantMessage), nil
}

func (s *Service) writeLayer(ctx context.Context, id, before, after string, current *definition.Unit) error {
	layersRoot := filepath.Join(s.Definition.Root, "layers")
	temporary, err := os.MkdirTemp(layersRoot, ".layerctl-"+id+"-")
	if err != nil {
		return fmt.Errorf("create temporary layer: %w", err)
	}
	defer os.RemoveAll(temporary)

	message, err := commitMessage(s.Git, ctx, s.Git.Root, after)
	if err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(temporary, "COMMIT_MSG"), message, 0o644); err != nil {
		return fmt.Errorf("write commit message: %w", err)
	}

	paths, err := changedPaths(s.Git, ctx, s.Git.Root, before, after)
	if err != nil {
		return err
	}
	var patchPaths []string
	for _, path := range paths {
		beforeExists, err := objectExists(s.Git, ctx, s.Git.Root, before, path)
		if err != nil {
			return err
		}
		afterExists, err := objectExists(s.Git, ctx, s.Git.Root, after, path)
		if err != nil {
			return err
		}
		if !beforeExists && afterExists {
			if err := extractOverlay(s.Git, ctx, s.Git.Root, after, path, filepath.Join(temporary, "overlay", path)); err != nil {
				return err
			}
			continue
		}
		patchPaths = append(patchPaths, path)
	}
	if len(patchPaths) > 0 {
		patch, err := s.Git.Bytes(ctx, s.Git.Root, append([]string{"diff", "--binary", "--full-index", before, after, "--"}, patchPaths...)...)
		if err != nil {
			return fmt.Errorf("generate inherited-path patch: %w", err)
		}
		patchDirectory := filepath.Join(temporary, "patches")
		if err := os.MkdirAll(patchDirectory, 0o755); err != nil {
			return fmt.Errorf("create patch directory: %w", err)
		}
		if err := os.WriteFile(filepath.Join(patchDirectory, "001-changes.patch"), patch, 0o644); err != nil {
			return fmt.Errorf("write generated patch: %w", err)
		}
	}
	if len(paths) == 0 {
		return errors.New("source projection has no tree changes to capture")
	}

	target := filepath.Join(layersRoot, id)
	if current != nil {
		target = current.Directory
	}
	if current == nil {
		if err := os.Rename(temporary, target); err != nil {
			return fmt.Errorf("install layer %q: %w", id, err)
		}
		return nil
	}
	backup, err := os.MkdirTemp(layersRoot, ".layerctl-backup-"+id+"-")
	if err != nil {
		return fmt.Errorf("reserve layer backup: %w", err)
	}
	if err := os.Remove(backup); err != nil {
		return fmt.Errorf("prepare layer backup: %w", err)
	}
	if err := os.Rename(target, backup); err != nil {
		return fmt.Errorf("backup layer %q: %w", id, err)
	}
	if err := os.Rename(temporary, target); err != nil {
		_ = os.Rename(backup, target)
		return fmt.Errorf("install refreshed layer %q: %w", id, err)
	}
	if err := os.RemoveAll(backup); err != nil {
		return fmt.Errorf("remove layer backup %q: %w", backup, err)
	}
	return nil
}

func commitMessage(git *gitrepo.Repository, ctx context.Context, root, commit string) ([]byte, error) {
	object, err := git.Bytes(ctx, root, "cat-file", "commit", commit)
	if err != nil {
		return nil, fmt.Errorf("read commit %q: %w", commit, err)
	}
	_, message, ok := bytes.Cut(object, []byte("\n\n"))
	if !ok || len(message) == 0 {
		return nil, fmt.Errorf("commit %q has no message", commit)
	}
	return message, nil
}

func changedPaths(git *gitrepo.Repository, ctx context.Context, root, before, after string) ([]string, error) {
	output, err := git.Bytes(ctx, root, "diff", "--name-only", "-z", before, after)
	if err != nil {
		return nil, fmt.Errorf("list changed paths: %w", err)
	}
	fields := bytes.Split(output, []byte{0})
	paths := make([]string, 0, len(fields))
	for _, field := range fields {
		if len(field) > 0 {
			paths = append(paths, string(field))
		}
	}
	return paths, nil
}

func objectExists(git *gitrepo.Repository, ctx context.Context, root, commit, path string) (bool, error) {
	_, err := git.Bytes(ctx, root, "cat-file", "-e", commit+":"+path)
	if err != nil {
		return false, nil
	}
	return true, nil
}

func extractOverlay(git *gitrepo.Repository, ctx context.Context, root, commit, path, target string) error {
	entry, err := git.Output(ctx, root, "ls-tree", commit, "--", path)
	if err != nil {
		return fmt.Errorf("inspect overlay object %q: %w", path, err)
	}
	fields := strings.Fields(entry)
	if len(fields) < 3 {
		return fmt.Errorf("cannot parse tree entry for %q", path)
	}
	content, err := git.Bytes(ctx, root, "cat-file", "blob", commit+":"+path)
	if err != nil {
		return fmt.Errorf("read overlay object %q: %w", path, err)
	}
	if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
		return fmt.Errorf("create overlay parent for %q: %w", path, err)
	}
	switch fields[0] {
	case "100644":
		if err := os.WriteFile(target, content, 0o644); err != nil {
			return fmt.Errorf("write overlay file %q: %w", path, err)
		}
	case "100755":
		if err := os.WriteFile(target, content, 0o755); err != nil {
			return fmt.Errorf("write executable overlay file %q: %w", path, err)
		}
	case "120000":
		if err := os.Symlink(string(content), target); err != nil {
			return fmt.Errorf("write overlay symlink %q: %w", path, err)
		}
	default:
		return fmt.Errorf("unsupported Git mode %q for overlay path %q", fields[0], path)
	}
	return nil
}
