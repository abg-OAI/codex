package definition_test

import (
	"bytes"
	"os"
	"path/filepath"
	"testing"

	"github.com/abg-OAI/codex/layerctl/internal/definition"
)

func TestLoad_overlayPreservesBlobKinds(t *testing.T) {
	root := t.TempDir()
	writeFile(t, filepath.Join(root, "upstream.json"), `{"tag":"rust-v1.2.3","commit":"abc123"}`)
	writeLayer(t, root, "0000-foundation", false)
	entries := []definition.OverlayEntry{
		{Path: "binary", Mode: "100644", Data: []byte{0, 255, 1}},
		{Path: "empty", Mode: "100755", Data: []byte{}},
		{Path: "link", Mode: "120000", Data: []byte("../../outside")},
		{Path: "nested/file", Mode: "100644", Data: []byte("source\n")},
	}
	directory := filepath.Join(root, "layers", "0000-foundation")
	if err := definition.WriteOverlay(directory, entries); err != nil {
		t.Fatal(err)
	}
	loaded, err := definition.Load(root)
	if err != nil {
		t.Fatal(err)
	}
	got := loaded.Layers[0].Overlay
	if len(got) != len(entries) {
		t.Fatalf("overlay entries = %d", len(got))
	}
	for i, want := range entries {
		if got[i].Path != want.Path || got[i].Mode != want.Mode || !bytes.Equal(got[i].Data, want.Data) {
			t.Fatalf("entry %d = %+v, want %+v", i, got[i], want)
		}
	}
}

func TestOverlayEntry_rejectsUnsafePaths(t *testing.T) {
	for _, path := range []string{"../outside", "/absolute", ".git/config", "parent/.GiT/config", "a/../b"} {
		t.Run(path, func(t *testing.T) {
			entry := definition.OverlayEntry{Path: path, Mode: "100644"}
			if err := entry.Validate(); err == nil {
				t.Fatal("unsafe path accepted")
			}
		})
	}
}

func TestLoad_rejectsOverlayRootSymlinkAndExec(t *testing.T) {
	root := t.TempDir()
	writeFile(t, filepath.Join(root, "upstream.json"), `{"tag":"rust-v1.2.3","commit":"abc123"}`)
	writeLayer(t, root, "0000-foundation", false)
	directory := filepath.Join(root, "layers", "0000-foundation")
	if err := os.Symlink(t.TempDir(), filepath.Join(directory, "overlay")); err != nil {
		t.Fatal(err)
	}
	if _, err := definition.Load(root); err == nil {
		t.Fatal("overlay root symlink accepted")
	}
	if err := os.Remove(filepath.Join(directory, "overlay")); err != nil {
		t.Fatal(err)
	}
	writeFile(t, filepath.Join(directory, "exec"), "exit 0\n")
	if _, err := definition.Load(root); err == nil {
		t.Fatal("unsupported exec accepted")
	}
}
