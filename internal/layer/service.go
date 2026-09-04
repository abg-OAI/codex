// Package layer captures accepted projection commits as canonical Saffrodex
// layer patches.
package layer

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"

	"github.com/abg-OAI/codex/layerctl/internal/definition"
	"github.com/abg-OAI/codex/layerctl/internal/gitrepo"
	"github.com/abg-OAI/codex/layerctl/internal/mailpatch"
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
	return s.writeLayer(ctx, req.ID, source.Base, source.Head, false)
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
	return s.writeLayer(ctx, id, parent, source.Head, true)
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
	patches := &mailpatch.Service{Git: s.Git}
	wantMessage, err := patches.Message(ctx, unit.PatchPath)
	if err != nil {
		return false, err
	}
	return bytes.Equal(message, wantMessage), nil
}

func (s *Service) writeLayer(ctx context.Context, id, before, after string, replaceExisting bool) error {
	layersRoot := filepath.Join(s.Definition.Root, "layers")
	patches := &mailpatch.Service{Git: s.Git}
	content, err := patches.Capture(ctx, mailpatch.CaptureRequest{
		Before: before,
		After:  after,
	})
	if err != nil {
		return err
	}

	temporary, err := os.CreateTemp(layersRoot, ".layerctl-"+id+"-*.patch")
	if err != nil {
		return fmt.Errorf("create temporary layer patch: %w", err)
	}
	temporaryPath := temporary.Name()
	defer os.Remove(temporaryPath)
	if _, err := temporary.Write(content); err != nil {
		_ = temporary.Close()
		return fmt.Errorf("write temporary layer patch: %w", err)
	}
	if err := temporary.Close(); err != nil {
		return fmt.Errorf("close temporary layer patch: %w", err)
	}
	matches, err := patches.Matches(ctx, temporaryPath, before, after)
	if err != nil {
		return fmt.Errorf("verify captured layer %q: %w", id, err)
	}
	if !matches {
		return fmt.Errorf("captured layer %q does not reproduce the accepted commit", id)
	}

	target := filepath.Join(layersRoot, id+".patch")
	if !replaceExisting {
		if err := os.Rename(temporaryPath, target); err != nil {
			return fmt.Errorf("install layer %q: %w", id, err)
		}
		return nil
	}
	backup, err := os.CreateTemp(layersRoot, ".layerctl-backup-"+id+"-*.patch")
	if err != nil {
		return fmt.Errorf("reserve layer backup: %w", err)
	}
	backupPath := backup.Name()
	if err := backup.Close(); err != nil {
		return fmt.Errorf("close layer backup: %w", err)
	}
	if err := os.Remove(backupPath); err != nil {
		return fmt.Errorf("prepare layer backup: %w", err)
	}
	if err := os.Rename(target, backupPath); err != nil {
		return fmt.Errorf("backup layer %q: %w", id, err)
	}
	if err := os.Rename(temporaryPath, target); err != nil {
		_ = os.Rename(backupPath, target)
		return fmt.Errorf("install refreshed layer %q: %w", id, err)
	}
	if err := os.Remove(backupPath); err != nil {
		return fmt.Errorf("remove layer backup %q: %w", backupPath, err)
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
