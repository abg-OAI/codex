package definition

import (
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
)

// OverlayEntry is a final added Git blob. Mode is 100644, 100755 or 120000;
// symlink Data contains its target, never the contents of the target file.
type OverlayEntry struct {
	Path string
	Mode string
	Data []byte
}

// Validate rejects paths that could address Git metadata or escape the tree.
func (e OverlayEntry) Validate() error {
	if !fs.ValidPath(e.Path) || e.Path == "." || !filepath.IsLocal(e.Path) {
		return fmt.Errorf("invalid overlay path %q", e.Path)
	}
	for _, component := range strings.Split(e.Path, "/") {
		if strings.EqualFold(component, ".git") {
			return fmt.Errorf("overlay path %q addresses Git metadata", e.Path)
		}
	}
	switch e.Mode {
	case "100644", "100755", "120000":
		return nil
	default:
		return fmt.Errorf("unsupported overlay mode %q for %q", e.Mode, e.Path)
	}
}

func readOverlay(directory string) ([]OverlayEntry, error) {
	root, err := os.OpenRoot(directory)
	if err != nil {
		return nil, fmt.Errorf("open overlay: %w", err)
	}
	defer root.Close()
	var entries []OverlayEntry
	err = fs.WalkDir(root.FS(), ".", func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			return nil
		}
		info, err := d.Info()
		if err != nil {
			return err
		}
		entry := OverlayEntry{Path: path, Mode: "100644"}
		switch {
		case info.Mode()&os.ModeSymlink != 0:
			entry.Mode = "120000"
			target, err := root.Readlink(path)
			if err != nil {
				return err
			}
			entry.Data = []byte(target)
		case info.Mode().IsRegular():
			if info.Mode()&0o111 != 0 {
				entry.Mode = "100755"
			}
			entry.Data, err = root.ReadFile(path)
			if err != nil {
				return err
			}
		default:
			return fmt.Errorf("unsupported overlay file %q", path)
		}
		if err := entry.Validate(); err != nil {
			return err
		}
		entries = append(entries, entry)
		return nil
	})
	if err != nil {
		return nil, fmt.Errorf("read overlay %q: %w", directory, err)
	}
	return entries, nil
}

// WriteOverlay serializes additions into a new layer directory. Existing leaves
// are never replaced; callers publish the complete temporary directory atomically.
func WriteOverlay(directory string, entries []OverlayEntry) error {
	if len(entries) == 0 {
		return nil
	}
	directory = filepath.Join(directory, "overlay")
	if err := os.MkdirAll(directory, 0o755); err != nil {
		return err
	}
	root, err := os.OpenRoot(directory)
	if err != nil {
		return err
	}
	defer root.Close()
	for _, entry := range entries {
		if err := entry.Validate(); err != nil {
			return err
		}
		if err := root.MkdirAll(filepath.Dir(entry.Path), 0o755); err != nil {
			return err
		}
		if entry.Mode == "120000" {
			if err := root.Symlink(string(entry.Data), entry.Path); err != nil {
				return err
			}
			continue
		}
		mode := os.FileMode(0o644)
		if entry.Mode == "100755" {
			mode = 0o755
		}
		file, err := root.OpenFile(entry.Path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, mode)
		if err != nil {
			return err
		}
		_, err = file.Write(entry.Data)
		closeErr := file.Close()
		if err != nil {
			return err
		}
		if closeErr != nil {
			return closeErr
		}
	}
	return nil
}
