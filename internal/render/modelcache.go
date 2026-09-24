package render

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"os"
	"path"
	"path/filepath"
	"strings"
	"time"
)

const (
	kicadGitHubRaw = "https://raw.githubusercontent.com/KiCad/kicad-packages3D/master/"
	kicadGitLabRaw = "https://gitlab.com/kicad/libraries/kicad-packages3D/-/raw/master/"
)

// defaultModelCache is $XDG_CACHE_HOME/fragua/3d, or ~/.cache/fragua/3d.
// KiCad WRLs land in kicad-packages3d/ under that root; EasyEDA OBJs in easyeda/.
func defaultModelCache() string {
	if x := strings.TrimSpace(os.Getenv("XDG_CACHE_HOME")); x != "" {
		return filepath.Join(x, "fragua", "3d")
	}
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return filepath.Join(os.TempDir(), "fragua", "3d")
	}
	return filepath.Join(home, ".cache", "fragua", "3d")
}

func kicadInstallDirs() []string {
	var out []string
	for _, env := range []string{"KICAD9_3DMODEL_DIR", "KICAD8_3DMODEL_DIR", "KICAD7_3DMODEL_DIR", "KISYS3DMOD"} {
		v := strings.TrimSpace(os.Getenv(env))
		if v == "" {
			continue
		}
		for _, p := range filepath.SplitList(v) {
			if p = strings.TrimSpace(p); p != "" {
				out = append(out, p)
			}
		}
	}
	out = append(out, "/usr/share/kicad/3dmodels")
	return out
}

// safeRel rejects absolute paths and any `..` segment. rel uses forward slashes.
func safeRel(p string) (string, error) {
	p = strings.TrimSpace(strings.ReplaceAll(p, "\\", "/"))
	p = strings.TrimPrefix(p, "${KICAD9_3DMODEL_DIR}/")
	p = strings.TrimPrefix(p, "${KICAD8_3DMODEL_DIR}/")
	p = strings.TrimPrefix(p, "${KICAD7_3DMODEL_DIR}/")
	p = strings.TrimPrefix(p, "${KISYS3DMOD}/")
	if p == "" || strings.Contains(p, "\x00") {
		return "", fmt.Errorf("empty model path")
	}
	if strings.HasPrefix(p, "/") || filepath.IsAbs(p) {
		return "", fmt.Errorf("model path %q is absolute", p)
	}
	clean := path.Clean(p)
	if clean == "." || clean == ".." || strings.HasPrefix(clean, "../") {
		return "", fmt.Errorf("model path %q escapes the cache", p)
	}
	return clean, nil
}

func isLocalModel(m string) bool {
	m = strings.TrimSpace(m)
	if m == "" {
		return false
	}
	if filepath.IsAbs(m) || strings.HasPrefix(m, ".") || strings.HasPrefix(m, "~") {
		return true
	}
	st, err := os.Stat(m)
	return err == nil && !st.IsDir()
}

type fetchFunc func(ctx context.Context, url string) ([]byte, error)

func (l *loader) readKiCad(rel string) ([]byte, string, error) {
	rel, err := safeRel(rel)
	if err != nil {
		return nil, "", err
	}
	if b, where, ok := l.readCached(rel); ok {
		return b, where, nil
	}
	if l.opt.Offline {
		return nil, "", fmt.Errorf("offline: %s is not in the cache or a local KiCad install", rel)
	}
	var last error
	for _, base := range []string{kicadGitHubRaw, kicadGitLabRaw} {
		u := base + escapeRel(rel)
		b, err := l.fetch(u)
		if err != nil {
			last = err
			continue
		}
		dest := filepath.Join(l.cacheRoot(), "kicad-packages3d", filepath.FromSlash(rel))
		if err := atomicWrite(dest, b); err != nil {
			return b, u, nil
		}
		return b, dest, nil
	}
	if last == nil {
		last = fmt.Errorf("download failed")
	}
	return nil, "", fmt.Errorf("%s: %w", rel, last)
}

func (l *loader) readCached(rel string) ([]byte, string, bool) {
	dirs := kicadInstallDirs()
	if l.opt.SearchDirs != nil {
		dirs = l.opt.SearchDirs
	}
	for _, dir := range dirs {
		p := filepath.Join(dir, filepath.FromSlash(rel))
		if b, err := os.ReadFile(p); err == nil && len(b) > 0 {
			return b, p, true
		}
	}
	root := l.cacheRoot()
	for _, p := range []string{
		filepath.Join(root, filepath.FromSlash(rel)),
		filepath.Join(root, "kicad-packages3d", filepath.FromSlash(rel)),
	} {
		if b, err := os.ReadFile(p); err == nil && len(b) > 0 {
			return b, p, true
		}
	}
	return nil, "", false
}

func (l *loader) cacheRoot() string {
	if strings.TrimSpace(l.opt.CacheDir) != "" {
		return l.opt.CacheDir
	}
	return defaultModelCache()
}

func (l *loader) fetch(url string) ([]byte, error) {
	fn := l.get
	if fn == nil {
		fn = httpGet
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	return fn(ctx, url)
}

func httpGet(ctx context.Context, url string) ([]byte, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("User-Agent", easyedaUA)
	req.Header.Set("Accept", "*/*")
	client := &http.Client{Timeout: 30 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(io.LimitReader(resp.Body, 20<<20))
	if err != nil {
		return nil, err
	}
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("HTTP %d", resp.StatusCode)
	}
	if len(body) == 0 {
		return nil, fmt.Errorf("empty body")
	}
	return body, nil
}

func escapeRel(rel string) string {
	parts := strings.Split(rel, "/")
	for i, p := range parts {
		parts[i] = urlPathEscape(p)
	}
	return strings.Join(parts, "/")
}

func urlPathEscape(s string) string {
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		c := s[i]
		if (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') ||
			c == '-' || c == '_' || c == '.' || c == '~' {
			b.WriteByte(c)
			continue
		}
		fmt.Fprintf(&b, "%%%02X", c)
	}
	return b.String()
}

func atomicWrite(dest string, data []byte) error {
	if err := os.MkdirAll(filepath.Dir(dest), 0o755); err != nil {
		return err
	}
	tmp := dest + ".tmp"
	if err := os.WriteFile(tmp, data, 0o644); err != nil {
		return err
	}
	return os.Rename(tmp, dest)
}
