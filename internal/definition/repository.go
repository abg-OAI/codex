// Package definition reads and validates the canonical Saffrodex repository.
// It owns the on-disk layer representation, while projection and maintenance
// packages own operations performed with those definitions.
package definition

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"slices"
)

var (
	layerIDPattern = regexp.MustCompile(`^[0-9]{4}-[a-z0-9]+(-[a-z0-9]+)*$`)
	patchPattern   = regexp.MustCompile(`^[0-9]{3}-.+\.patch$`)
)

// FoundationLayerID identifies the required first generated commit.
const FoundationLayerID = "0000-foundation"

// Repository is one complete canonical Saffrodex definition.
// Layers are ordered lexically by their numbered directory names.
type Repository struct {
	Root     string
	Upstream Upstream
	Layers   []Unit
}

// Upstream identifies the annotated or lightweight release tag and the commit
// it must resolve to. Keeping both values detects a locally moved tag.
type Upstream struct {
	Tag    string `json:"tag"`
	Commit string `json:"commit"`
}

// Unit is one generated commit. Overlay paths must not exist before the unit,
// and patches modify or delete paths present at that point in the projection.
type Unit struct {
	ID                string
	Directory         string
	CommitMessagePath string
	OverlayPath       string
	PatchPaths        []string
}

// Load reads the canonical repository rooted at root and rejects malformed or
// ambiguous layer definitions before a projection mutates Git state.
func Load(root string) (*Repository, error) {
	absRoot, err := filepath.Abs(root)
	if err != nil {
		return nil, fmt.Errorf("resolve repository root %q: %w", root, err)
	}

	upstream, err := loadUpstream(filepath.Join(absRoot, "upstream.json"))
	if err != nil {
		return nil, err
	}

	layers, err := loadLayers(filepath.Join(absRoot, "layers"))
	if err != nil {
		return nil, err
	}
	if len(layers) == 0 || layers[0].ID != FoundationLayerID {
		return nil, fmt.Errorf("first layer must be %q", FoundationLayerID)
	}

	return &Repository{
		Root:     absRoot,
		Upstream: upstream,
		Layers:   layers,
	}, nil
}

func loadUpstream(path string) (Upstream, error) {
	file, err := os.Open(path)
	if err != nil {
		return Upstream{}, fmt.Errorf("open %q: %w", path, err)
	}
	defer file.Close()

	var upstream Upstream
	decoder := json.NewDecoder(file)
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&upstream); err != nil {
		return Upstream{}, fmt.Errorf("decode %q: %w", path, err)
	}
	if err := requireJSONEnd(decoder); err != nil {
		return Upstream{}, fmt.Errorf("decode %q: %w", path, err)
	}
	if upstream.Tag == "" {
		return Upstream{}, errors.New("upstream tag is required")
	}
	if upstream.Commit == "" {
		return Upstream{}, errors.New("upstream commit is required")
	}
	return upstream, nil
}

func requireJSONEnd(decoder *json.Decoder) error {
	var extra any
	if err := decoder.Decode(&extra); !errors.Is(err, io.EOF) {
		if err == nil {
			return errors.New("unexpected value after object")
		}
		return err
	}
	return nil
}

func loadLayers(path string) ([]Unit, error) {
	entries, err := os.ReadDir(path)
	if errors.Is(err, os.ErrNotExist) {
		return []Unit{}, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read layer directory %q: %w", path, err)
	}
	layers := make([]Unit, 0, len(entries))
	for _, entry := range entries {
		id := entry.Name()
		if !entry.IsDir() || !layerIDPattern.MatchString(id) {
			return nil, fmt.Errorf("invalid layer entry %q", filepath.Join(path, id))
		}
		unit, err := loadUnit(id, filepath.Join(path, id))
		if err != nil {
			return nil, err
		}
		layers = append(layers, unit)
	}
	return layers, nil
}

// ValidateLayerID rejects names that cannot be canonical layer directories.
func ValidateLayerID(id string) error {
	if !layerIDPattern.MatchString(id) {
		return fmt.Errorf("invalid layer ID %q", id)
	}
	return nil
}

func loadUnit(id, directory string) (Unit, error) {
	if err := ValidateLayerID(id); err != nil {
		return Unit{}, err
	}

	messagePath := filepath.Join(directory, "COMMIT_MSG")
	message, err := os.ReadFile(messagePath)
	if err != nil {
		return Unit{}, fmt.Errorf("read %q: %w", messagePath, err)
	}
	if len(message) == 0 {
		return Unit{}, fmt.Errorf("commit message %q is empty", messagePath)
	}

	overlayPath := filepath.Join(directory, "overlay")
	if info, err := os.Stat(overlayPath); err != nil {
		if !errors.Is(err, os.ErrNotExist) {
			return Unit{}, fmt.Errorf("inspect overlay %q: %w", overlayPath, err)
		}
		overlayPath = ""
	} else if !info.IsDir() {
		return Unit{}, fmt.Errorf("overlay %q is not a directory", overlayPath)
	}

	patchDirectory := filepath.Join(directory, "patches")
	entries, err := os.ReadDir(patchDirectory)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return Unit{}, fmt.Errorf("read patch directory %q: %w", patchDirectory, err)
	}
	patches := make([]string, 0, len(entries))
	for _, entry := range entries {
		if entry.IsDir() || !patchPattern.MatchString(entry.Name()) {
			return Unit{}, fmt.Errorf("invalid patch entry %q", filepath.Join(patchDirectory, entry.Name()))
		}
		patches = append(patches, filepath.Join(patchDirectory, entry.Name()))
	}
	slices.Sort(patches)
	if overlayPath == "" && len(patches) == 0 {
		return Unit{}, fmt.Errorf("unit %q has no overlay or patches", id)
	}

	return Unit{
		ID:                id,
		Directory:         directory,
		CommitMessagePath: messagePath,
		OverlayPath:       overlayPath,
		PatchPaths:        patches,
	}, nil
}
