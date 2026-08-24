package definition_test

import (
	"os"
	"path/filepath"
	"reflect"
	"testing"

	"github.com/abg-OAI/codex/layerctl/internal/definition"
)

func TestLoadSortsNumberedLayersAndPatches(t *testing.T) {
	root := t.TempDir()
	writeFile(t, filepath.Join(root, "upstream.json"), `{"tag":"rust-v1.2.3","commit":"abc123"}`)
	writeFile(t, filepath.Join(root, "layers", "0000-foundation", "COMMIT_MSG"), "foundation\n")
	writeFile(t, filepath.Join(root, "layers", "0000-foundation", "overlay", "foundation.txt"), "foundation\n")
	writeFile(t, filepath.Join(root, "layers", "0002-second", "COMMIT_MSG"), "second\n")
	writeFile(t, filepath.Join(root, "layers", "0002-second", "patches", "020-later.patch"), "later\n")
	writeFile(t, filepath.Join(root, "layers", "0002-second", "patches", "010-earlier.patch"), "earlier\n")
	writeFile(t, filepath.Join(root, "layers", "0001-first", "COMMIT_MSG"), "first\n")
	writeFile(t, filepath.Join(root, "layers", "0001-first", "overlay", "first.txt"), "first\n")

	repository, err := definition.Load(root)
	if err != nil {
		t.Fatalf("Load() error = %v", err)
	}

	gotLayers := []string{repository.Layers[0].ID, repository.Layers[1].ID, repository.Layers[2].ID}
	if want := []string{"0000-foundation", "0001-first", "0002-second"}; !reflect.DeepEqual(gotLayers, want) {
		t.Fatalf("layer order = %v, want %v", gotLayers, want)
	}
	gotPatches := []string{
		filepath.Base(repository.Layers[2].PatchPaths[0]),
		filepath.Base(repository.Layers[2].PatchPaths[1]),
	}
	if want := []string{"010-earlier.patch", "020-later.patch"}; !reflect.DeepEqual(gotPatches, want) {
		t.Fatalf("patch order = %v, want %v", gotPatches, want)
	}
}

func TestLoadRejectsUnnumberedLayer(t *testing.T) {
	root := t.TempDir()
	writeFile(t, filepath.Join(root, "upstream.json"), `{"tag":"rust-v1.2.3","commit":"abc123"}`)
	writeFile(t, filepath.Join(root, "layers", "0000-foundation", "COMMIT_MSG"), "foundation\n")
	writeFile(t, filepath.Join(root, "layers", "0000-foundation", "overlay", "foundation.txt"), "foundation\n")
	writeFile(t, filepath.Join(root, "layers", "feature", "COMMIT_MSG"), "feature\n")
	writeFile(t, filepath.Join(root, "layers", "feature", "overlay", "feature.txt"), "feature\n")

	if _, err := definition.Load(root); err == nil {
		t.Fatal("Load() error = nil, want invalid layer directory error")
	}
}

func TestLoadAllowsRepositoryWithoutProductLayers(t *testing.T) {
	root := t.TempDir()
	writeFile(t, filepath.Join(root, "upstream.json"), `{"tag":"rust-v1.2.3","commit":"abc123"}`)
	writeFile(t, filepath.Join(root, "layers", "0000-foundation", "COMMIT_MSG"), "foundation\n")
	writeFile(t, filepath.Join(root, "layers", "0000-foundation", "overlay", "foundation.txt"), "foundation\n")

	repository, err := definition.Load(root)
	if err != nil {
		t.Fatalf("Load() error = %v", err)
	}
	if got, want := len(repository.Layers), 1; got != want {
		t.Fatalf("len(Load().Layers) = %d, want %d", got, want)
	}
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
