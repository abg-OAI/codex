package upstream

import (
	"bufio"
	"bytes"
	"context"
	"fmt"
	"sort"
	"strconv"
	"strings"
)

const cargoLockPath = "codex-rs/Cargo.lock"

type cargoLockPackage struct {
	name      string
	version   string
	hasSource bool
}

func (s *Service) checkCargoLockVersions(ctx context.Context, generatedCommit string) error {
	upstreamLock, err := s.Git.Bytes(
		ctx,
		s.Git.Root,
		"show",
		s.Definition.Upstream.Commit+":"+cargoLockPath,
	)
	if err != nil {
		return fmt.Errorf("read upstream %s: %w", cargoLockPath, err)
	}
	generatedLock, err := s.Git.Bytes(
		ctx,
		s.Git.Root,
		"show",
		generatedCommit+":"+cargoLockPath,
	)
	if err != nil {
		return fmt.Errorf("read generated %s: %w", cargoLockPath, err)
	}
	if err := validateCargoLockVersions(upstreamLock, generatedLock); err != nil {
		return fmt.Errorf("validate generated %s: %w", cargoLockPath, err)
	}
	return nil
}

func validateCargoLockVersions(upstreamLock, generatedLock []byte) error {
	upstreamPackages, err := parseCargoLockPackages(upstreamLock)
	if err != nil {
		return fmt.Errorf("parse upstream lockfile: %w", err)
	}
	generatedPackages, err := parseCargoLockPackages(generatedLock)
	if err != nil {
		return fmt.Errorf("parse generated lockfile: %w", err)
	}

	upstreamVersions := make(map[string]map[string]struct{})
	for _, pkg := range upstreamPackages {
		if pkg.hasSource {
			continue
		}
		versions := upstreamVersions[pkg.name]
		if versions == nil {
			versions = make(map[string]struct{})
			upstreamVersions[pkg.name] = versions
		}
		versions[pkg.version] = struct{}{}
	}

	var changed []string
	for _, pkg := range generatedPackages {
		if pkg.hasSource {
			continue
		}
		versions, existedUpstream := upstreamVersions[pkg.name]
		_, versionMatches := versions[pkg.version]
		if existedUpstream && versionMatches {
			continue
		}
		if !existedUpstream && pkg.version == "0.0.0" {
			continue
		}
		changed = append(changed, fmt.Sprintf("%s=%s", pkg.name, pkg.version))
	}
	if len(changed) == 0 {
		return nil
	}

	sort.Strings(changed)
	return fmt.Errorf(
		"local package versions differ from upstream or the 0.0.0 development version: %s",
		strings.Join(changed, ", "),
	)
}

func parseCargoLockPackages(content []byte) ([]cargoLockPackage, error) {
	scanner := bufio.NewScanner(bytes.NewReader(content))
	packages := make([]cargoLockPackage, 0)
	var current *cargoLockPackage
	lineNumber := 0
	flush := func() error {
		if current == nil {
			return nil
		}
		if current.name == "" || current.version == "" {
			return fmt.Errorf("package entry is missing name or version")
		}
		packages = append(packages, *current)
		return nil
	}

	for scanner.Scan() {
		lineNumber++
		line := strings.TrimSpace(scanner.Text())
		if line == "[[package]]" {
			if err := flush(); err != nil {
				return nil, fmt.Errorf("line %d: %w", lineNumber, err)
			}
			current = &cargoLockPackage{}
			continue
		}
		if current == nil {
			continue
		}
		var fieldErr error
		switch {
		case strings.HasPrefix(line, "name = "):
			current.name, fieldErr = cargoLockString(line, "name")
		case strings.HasPrefix(line, "version = "):
			current.version, fieldErr = cargoLockString(line, "version")
		case strings.HasPrefix(line, "source = "):
			current.hasSource = true
		}
		if fieldErr != nil {
			return nil, fmt.Errorf("line %d: %w", lineNumber, fieldErr)
		}
	}
	if err := scanner.Err(); err != nil {
		return nil, fmt.Errorf("scan lockfile: %w", err)
	}
	if err := flush(); err != nil {
		return nil, fmt.Errorf("end of file: %w", err)
	}
	return packages, nil
}

func cargoLockString(line, key string) (string, error) {
	value := strings.TrimSpace(strings.TrimPrefix(line, key+" = "))
	parsed, err := strconv.Unquote(value)
	if err != nil {
		return "", fmt.Errorf("parse %s: %w", key, err)
	}
	return parsed, nil
}
