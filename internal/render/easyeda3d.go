package render

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/mentasystems/fragua/internal/parts"
)

// easyedaUA matches the browser UA the component API requires. The 3D model
// host sits behind the same CloudFront check.
const easyedaUA = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 " +
	"(KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"

const easyedaModelURL = "https://modules.easyeda.com/3dmodel/%s"

// easyeda3DRef is the model uuid and the package's "rx,ry,rz" degree rotation.
type easyeda3DRef struct {
	UUID string
	Rot  string
}

// findEasyEDA3D walks an EasyEDA component JSON document. The useful uuid is
// the SVG node whose c_etype is outline3D (uuid_3d on the package head 404s
// on the model host for the parts we checked). Rotation is that node's
// c_rotation, for example "0,0,90".
func findEasyEDA3D(raw []byte) (easyeda3DRef, error) {
	var v any
	if err := json.Unmarshal(raw, &v); err != nil {
		return easyeda3DRef{}, err
	}
	if u, rot := walkOutline3D(v); u != "" {
		return easyeda3DRef{UUID: u, Rot: rot}, nil
	}
	if u := walkKey(v, "uuid_3d"); u != "" {
		return easyeda3DRef{UUID: u}, nil
	}
	return easyeda3DRef{}, fmt.Errorf("easyeda: no 3d model uuid")
}

func walkOutline3D(v any) (uuid, rot string) {
	switch t := v.(type) {
	case map[string]any:
		if et, _ := t["c_etype"].(string); et == "outline3D" {
			if u, _ := t["uuid"].(string); u != "" {
				r, _ := t["c_rotation"].(string)
				return u, r
			}
		}
		if attrs, ok := t["attrs"].(map[string]any); ok {
			if et, _ := attrs["c_etype"].(string); et == "outline3D" {
				if u, _ := attrs["uuid"].(string); u != "" {
					r, _ := attrs["c_rotation"].(string)
					return u, r
				}
			}
		}
		for _, c := range t {
			if u, r := walkOutline3D(c); u != "" {
				return u, r
			}
		}
	case []any:
		for _, c := range t {
			if u, r := walkOutline3D(c); u != "" {
				return u, r
			}
		}
	case string:
		if strings.Contains(t, "outline3D") {
			var inner any
			if json.Unmarshal([]byte(t), &inner) == nil {
				return walkOutline3D(inner)
			}
		}
	}
	return "", ""
}

func walkKey(v any, key string) string {
	switch t := v.(type) {
	case map[string]any:
		if s, ok := t[key].(string); ok && s != "" {
			return s
		}
		for _, c := range t {
			if s := walkKey(c, key); s != "" {
				return s
			}
		}
	case []any:
		for _, c := range t {
			if s := walkKey(c, key); s != "" {
				return s
			}
		}
	case string:
		if strings.Contains(t, key) {
			var inner any
			if json.Unmarshal([]byte(t), &inner) == nil {
				return walkKey(inner, key)
			}
		}
	}
	return ""
}

func parseEasyRot(s string) (rx, ry, rz float64) {
	parts := strings.Split(s, ",")
	if len(parts) != 3 {
		return 0, 0, 0
	}
	rx, _ = strconv.ParseFloat(strings.TrimSpace(parts[0]), 64)
	ry, _ = strconv.ParseFloat(strings.TrimSpace(parts[1]), 64)
	rz, _ = strconv.ParseFloat(strings.TrimSpace(parts[2]), 64)
	return rx, ry, rz
}

func (l *loader) readEasyEDA(lcsc string) (*cadMesh, string, error) {
	id, ok := parts.NormaliseLCSC(lcsc)
	if !ok {
		return nil, "", fmt.Errorf("easyeda: %q is not an LCSC id", lcsc)
	}
	objPath := filepath.Join(l.cacheRoot(), "easyeda", id+".obj")
	rotPath := objPath + ".rot"
	if b, err := os.ReadFile(objPath); err == nil && len(b) > 0 {
		mesh, err := parseModel(id+".obj", b)
		if err != nil {
			return nil, "", err
		}
		if rot, err := os.ReadFile(rotPath); err == nil {
			rx, ry, rz := parseEasyRot(string(rot))
			mesh.rotateEuler(rx, ry, rz)
		}
		return mesh, objPath, nil
	}
	if l.opt.Offline {
		return nil, "", fmt.Errorf("offline: no cached EasyEDA model for %s", id)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	raw, err := parts.HTTPFetcher{Timeout: 30 * time.Second}.Fetch(ctx, id)
	if err != nil {
		return nil, "", err
	}
	ref, err := findEasyEDA3D(raw)
	if err != nil {
		return nil, "", err
	}
	body, err := l.fetchEasyModel(ref.UUID)
	if err != nil {
		return nil, "", err
	}
	mesh, err := parseModel(id+".obj", body)
	if err != nil {
		return nil, "", err
	}
	_ = atomicWrite(objPath, body)
	_ = atomicWrite(rotPath, []byte(ref.Rot))
	rx, ry, rz := parseEasyRot(ref.Rot)
	mesh.rotateEuler(rx, ry, rz)
	return mesh, easyedaModelURLPrefix(ref.UUID), nil
}

func easyedaModelURLPrefix(uuid string) string {
	return fmt.Sprintf(easyedaModelURL, uuid)
}

func (l *loader) fetchEasyModel(uuid string) ([]byte, error) {
	if l.get != nil {
		return l.fetch(fmt.Sprintf(easyedaModelURL, uuid))
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, fmt.Sprintf(easyedaModelURL, uuid), nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("User-Agent", easyedaUA)
	req.Header.Set("Accept", "*/*")
	req.Header.Set("Referer", "https://easyeda.com/")
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
		return nil, fmt.Errorf("easyeda model %s: HTTP %d", uuid, resp.StatusCode)
	}
	if len(body) == 0 {
		return nil, fmt.Errorf("easyeda model %s: empty", uuid)
	}
	return body, nil
}
