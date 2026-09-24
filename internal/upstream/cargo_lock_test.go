package upstream

import (
	"strings"
	"testing"
)

func TestValidateCargoLockVersionsAcceptsDependencyAndDevelopmentPackageChanges(t *testing.T) {
	upstreamLock := []byte(`version = 4

[[package]]
name = "codex-core"
version = "0.0.0"

[[package]]
name = "path-tool"
version = "1.2.3"

[[package]]
name = "registry-package"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
`)
	generatedLock := []byte(`version = 4

[[package]]
name = "codex-core"
version = "0.0.0"

[[package]]
name = "path-tool"
version = "1.2.3"

[[package]]
name = "saffron-helper"
version = "0.0.0"

[[package]]
name = "registry-package"
version = "2.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
`)

	if err := validateCargoLockVersions(upstreamLock, generatedLock); err != nil {
		t.Fatalf("validateCargoLockVersions() error = %v", err)
	}
}

func TestValidateCargoLockVersionsRejectsLocalVersionChanges(t *testing.T) {
	upstreamLock := []byte(`version = 4

[[package]]
name = "codex-core"
version = "0.0.0"

[[package]]
name = "path-tool"
version = "1.2.3"
`)
	generatedLock := []byte(`version = 4

[[package]]
name = "codex-core"
version = "0.158.0-alpha.8"

[[package]]
name = "path-tool"
version = "2.0.0"

[[package]]
name = "saffron-helper"
version = "0.158.0-alpha.8"
`)

	err := validateCargoLockVersions(upstreamLock, generatedLock)
	if err == nil {
		t.Fatal("validateCargoLockVersions() error = nil")
	}
	for _, want := range []string{
		"codex-core=0.158.0-alpha.8",
		"path-tool=2.0.0",
		"saffron-helper=0.158.0-alpha.8",
	} {
		if !strings.Contains(err.Error(), want) {
			t.Errorf("validateCargoLockVersions() error = %q, want %q", err, want)
		}
	}
}
