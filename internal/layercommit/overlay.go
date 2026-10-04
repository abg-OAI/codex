package layercommit

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/abg-OAI/codex/layerctl/internal/definition"
	"github.com/abg-OAI/codex/layerctl/internal/gitrepo"
)

// OverlayCollisionError requires an operator to reconcile newly inherited
// paths, even when Git accepts identical additions without conflict markers.
// No commit is created. Cause retains application failures when the operator
// must reconstruct the complete desired layer rather than resolve markers.
type OverlayCollisionError struct {
	Paths []string
	Cause error
}

func (e *OverlayCollisionError) Error() string {
	if e.Cause != nil {
		return fmt.Sprintf("overlay paths collide with the predecessor: %q; reconcile the complete layer, then refresh: %v", e.Paths, e.Cause)
	}
	return fmt.Sprintf("overlay paths now exist in the predecessor: %q; resolve and refresh the layer", e.Paths)
}

func (e *OverlayCollisionError) Unwrap() error { return e.Cause }

// applicationPatch uses Git to encode overlay blobs as additions after the
// stored patch. No patch parser or filesystem overwrite semantics are needed.
func (s *Service) applicationPatch(ctx context.Context, unit definition.Unit) ([]byte, error) {
	var patch []byte
	if unit.PatchPath != "" {
		var err error
		patch, err = os.ReadFile(unit.PatchPath)
		if err != nil {
			return nil, err
		}
	}
	if len(unit.Overlay) == 0 {
		return patch, nil
	}
	// Git reports touched paths without interpreting patch syntax here.
	touched := make(map[string]bool)
	if len(patch) != 0 {
		stats, err := s.Git.Invoke(ctx, s.Git.Root, gitrepo.Invocation{
			Arguments: []string{"apply", "--numstat", "-z", "-"}, Stdin: patch,
		})
		if err != nil {
			return nil, fmt.Errorf("inspect patch paths: %w", err)
		}
		for record := range bytes.SplitSeq(stats, []byte{0}) {
			if len(record) == 0 {
				continue
			}
			fields := bytes.SplitN(record, []byte{'\t'}, 3)
			if len(fields) != 3 || len(fields[2]) == 0 {
				return nil, fmt.Errorf("unsupported patch path record %q", record)
			}
			touched[string(fields[2])] = true
		}
	}
	var additions bytes.Buffer
	for _, entry := range unit.Overlay {
		if err := entry.Validate(); err != nil {
			return nil, err
		}
		if touched[entry.Path] {
			return nil, fmt.Errorf("overlay path %q overlaps the patch or another addition", entry.Path)
		}
		touched[entry.Path] = true
		object, err := s.Git.Invoke(ctx, s.Git.Root, gitrepo.Invocation{
			Arguments: []string{"hash-object", "-w", "--stdin"}, Stdin: entry.Data,
		})
		if err != nil {
			return nil, err
		}
		fmt.Fprintf(&additions, "%s %s\t%s%c", entry.Mode, bytes.TrimSpace(object), entry.Path, 0)
	}
	empty, err := s.editTree(ctx, "", nil)
	if err != nil {
		return nil, err
	}
	overlay, err := s.editTree(ctx, "", additions.Bytes())
	if err != nil {
		return nil, err
	}
	delta, err := s.diff(ctx, empty, overlay)
	if err != nil {
		return nil, err
	}
	return append(append(patch, '\n'), delta...), nil
}

type treeEntry struct {
	Path   string
	Mode   string
	Object string
}

func (s *Service) treeEntries(ctx context.Context, tree string) (map[string]treeEntry, error) {
	output, err := s.Git.Bytes(ctx, s.Git.Root, "ls-tree", "-r", "-z", "--full-tree", tree)
	if err != nil {
		return nil, err
	}
	entries := make(map[string]treeEntry)
	for record := range bytes.SplitSeq(output, []byte{0}) {
		if len(record) == 0 {
			continue
		}
		header, path, ok := strings.Cut(string(record), "\t")
		fields := strings.Fields(header)
		if !ok || len(fields) != 3 {
			return nil, fmt.Errorf("invalid Git tree record %q", record)
		}
		entries[path] = treeEntry{Path: path, Mode: fields[0], Object: fields[2]}
	}
	return entries, nil
}

// editTree constructs a tree with a private index. It never changes the
// caller's worktree or index; updates use Git's NUL-delimited index-info format.
func (s *Service) editTree(ctx context.Context, base string, updates []byte) (string, error) {
	directory, err := os.MkdirTemp("", "layerctl-index-")
	if err != nil {
		return "", err
	}
	defer os.RemoveAll(directory)
	environment := []string{"GIT_INDEX_FILE=" + filepath.Join(directory, "index")}
	if base == "" {
		base = "--empty"
	}
	if _, err := s.Git.Invoke(ctx, s.Git.Root, gitrepo.Invocation{
		Arguments: []string{"read-tree", base}, Environment: environment,
	}); err != nil {
		return "", err
	}
	if len(updates) != 0 {
		if _, err := s.Git.Invoke(ctx, s.Git.Root, gitrepo.Invocation{
			Arguments: []string{"update-index", "-z", "--index-info"}, Stdin: updates, Environment: environment,
		}); err != nil {
			return "", err
		}
	}
	tree, err := s.Git.Invoke(ctx, s.Git.Root, gitrepo.Invocation{
		Arguments: []string{"write-tree"}, Environment: environment,
	})
	return strings.TrimSpace(string(tree)), err
}
