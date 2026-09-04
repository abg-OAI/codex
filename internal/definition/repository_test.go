package definition_test

import (
	"os"
	"path/filepath"
	"reflect"
	"testing"

	"github.com/abg-OAI/codex/layerctl/internal/definition"
)

func TestLoadSortsNumberedLayerPatches(t *testing.T) {
	root := t.TempDir()
	writeFile(t, filepath.Join(root, "upstream.json"), `{"tag":"rust-v1.2.3","commit":"abc123"}`)
	writeFile(t, filepath.Join(root, "layers", "0000-foundation.patch"), "foundation\n")
	writeFile(t, filepath.Join(root, "layers", "0002-second.patch"), "second\n")
	writeFile(t, filepath.Join(root, "layers", "0001-first.patch"), "first\n")

	repository, err := definition.Load(root)
	if err != nil {
		t.Fatalf("Load() error = %v", err)
	}

	gotLayers := []string{repository.Layers[0].ID, repository.Layers[1].ID, repository.Layers[2].ID}
	if want := []string{"0000-foundation", "0001-first", "0002-second"}; !reflect.DeepEqual(gotLayers, want) {
		t.Fatalf("layer order = %v, want %v", gotLayers, want)
	}
	if got, want := filepath.Base(repository.Layers[2].PatchPath), "0002-second.patch"; got != want {
		t.Fatalf("last patch = %q, want %q", got, want)
	}
}

func TestLoadRejectsUnnumberedLayer(t *testing.T) {
	root := t.TempDir()
	writeFile(t, filepath.Join(root, "upstream.json"), `{"tag":"rust-v1.2.3","commit":"abc123"}`)
	writeFile(t, filepath.Join(root, "layers", "0000-foundation.patch"), "foundation\n")
	writeFile(t, filepath.Join(root, "layers", "feature.patch"), "feature\n")

	if _, err := definition.Load(root); err == nil {
		t.Fatal("Load() error = nil, want invalid layer patch error")
	}
}

func TestLoadRejectsLegacyLayerDirectory(t *testing.T) {
	root := t.TempDir()
	writeFile(t, filepath.Join(root, "upstream.json"), `{"tag":"rust-v1.2.3","commit":"abc123"}`)
	writeFile(t, filepath.Join(root, "layers", "0000-foundation", "COMMIT_MSG"), "foundation\n")

	if _, err := definition.Load(root); err == nil {
		t.Fatal("Load() error = nil, want legacy layer directory error")
	}
}

func TestLoadAllowsRepositoryWithoutProductLayers(t *testing.T) {
	root := t.TempDir()
	writeFile(t, filepath.Join(root, "upstream.json"), `{"tag":"rust-v1.2.3","commit":"abc123"}`)
	writeFile(t, filepath.Join(root, "layers", "0000-foundation.patch"), "foundation\n")

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
