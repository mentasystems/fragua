package host

import (
	"encoding/json"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/mentasystems/fragua/internal/core"
)

// tinyBoard is the smallest project the check endpoints have something to
// say about: two parts, one net, no copper. lib-gen keeps it off ~/.pcb-library
// so CI runners (empty HOME) still spawn footprints.
const tinyBoard = `outline 20 12
lib-gen r_0603 family=chip size=0603 kind=r
sym U1 ic key=x
  pin 1 L VCC role=power_out
  pin 2 L GND role=power_in
sym R1 resistor key=r_0603
net SIG U1.VCC R1.1
palette U1 r_0603
palette R1 r_0603
place U1 6 6
place R1 14 6
`

func newTestServer(t *testing.T, script string) *httptest.Server {
	t.Helper()
	p := core.NewProject("api-test")
	srv := httptest.NewServer(Handler(p))
	t.Cleanup(srv.Close)
	if script != "" {
		r := post(t, srv, "/script", "text/plain", script)
		if r.status != 200 {
			t.Fatalf("setup script HTTP %d: %s", r.status, r.body)
		}
		if strings.Contains(string(r.body), "error line") {
			t.Fatalf("setup script: %s", r.body)
		}
	}
	return srv
}

func TestDRCAndERCJSON(t *testing.T) {
	srv := newTestServer(t, tinyBoard)
	for _, path := range []string{"/drc", "/erc"} {
		res := get(t, srv, path)
		if res.status != 200 {
			t.Fatalf("%s status %d", path, res.status)
		}
		if !strings.Contains(res.contentType, "application/json") {
			t.Fatalf("%s content-type %q", path, res.contentType)
		}
		var rep CheckReport
		if err := json.Unmarshal(res.body, &rep); err != nil {
			t.Fatalf("%s: %v (%s)", path, err, truncate(res.body, 200))
		}
		if rep.Source != strings.TrimPrefix(path, "/") {
			t.Fatalf("%s reported source %q", path, rep.Source)
		}
		if rep.Summary == "" {
			t.Fatalf("%s has no summary line", path)
		}
		for _, v := range rep.Violations {
			if v.ID == "" || v.Kind == "" || v.Severity == "" {
				t.Fatalf("%s violation missing id/kind/severity: %+v", path, v)
			}
		}
	}
}

func TestDRCEmptyProject(t *testing.T) {
	srv := newTestServer(t, "")
	var rep CheckReport
	res := get(t, srv, "/drc")
	if err := json.Unmarshal(res.body, &rep); err != nil {
		t.Fatalf("empty project /drc: %v", err)
	}
	if rep.Violations == nil {
		t.Fatal("violations must be [] not null, so the UI can iterate it")
	}
}

func TestSummary(t *testing.T) {
	srv := newTestServer(t, tinyBoard)
	res := get(t, srv, "/summary")
	if res.status != 200 {
		t.Fatalf("/summary status %d", res.status)
	}
	var s Summary
	if err := json.Unmarshal(res.body, &s); err != nil {
		t.Fatalf("/summary: %v (%s)", err, truncate(res.body, 200))
	}
	if s.Name != "api-test" {
		t.Fatalf("summary name %q", s.Name)
	}
	if s.WidthMM != 20 || s.HeightMM != 12 {
		t.Fatalf("summary outline %.1fx%.1f", s.WidthMM, s.HeightMM)
	}
	if len(s.Layers) != 2 || s.Layers[0] != "F.Cu" {
		t.Fatalf("summary layers %v", s.Layers)
	}
	if s.Parts != 2 {
		t.Fatalf("summary parts %d", s.Parts)
	}
	if s.Nets != 1 || s.NetsRouted != 0 || s.Unrouted != 1 {
		t.Fatalf("summary nets %d routed %d unrouted %d", s.Nets, s.NetsRouted, s.Unrouted)
	}
	if s.Op != nil {
		t.Fatalf("idle project should report no op, got %+v", s.Op)
	}
	// Routing the net moves it out of the ratsnest.
	if r := post(t, srv, "/script", "text/plain", "route max_seconds=10"); r.status != 200 {
		t.Fatalf("route HTTP %d", r.status)
	}
	res = get(t, srv, "/summary")
	_ = json.Unmarshal(res.body, &s)
	if s.NetsRouted != 1 || s.Unrouted != 0 {
		t.Fatalf("after route: routed %d unrouted %d", s.NetsRouted, s.Unrouted)
	}
}

func TestScreenshotMarkersAndSchematic(t *testing.T) {
	srv := newTestServer(t, tinyBoard)
	plain := get(t, srv, "/screenshot")
	assertPlausibleSVG(t, plain.body)
	if !strings.Contains(string(plain.body), `data-layer="drc"`) {
		t.Fatal("board SVG must always carry the drc group for the UI to fill")
	}
	if !strings.Contains(string(plain.body), `data-ref="U1"`) {
		t.Fatal("board SVG must address footprints by reference")
	}
	withDRC := get(t, srv, "/screenshot?drc=1")
	assertPlausibleSVG(t, withDRC.body)

	sch := get(t, srv, "/schematic")
	if sch.status != 200 || !strings.Contains(sch.contentType, "image/svg+xml") {
		t.Fatalf("/schematic: %d %q", sch.status, sch.contentType)
	}
	assertPlausibleSVG(t, sch.body)
	if !strings.Contains(string(sch.body), `data-sym="U1"`) {
		t.Fatalf("schematic SVG missing symbols: %s", truncate(sch.body, 200))
	}
}

func TestCancelIdle(t *testing.T) {
	srv := newTestServer(t, "")
	res := post(t, srv, "/cancel", "application/json", "")
	if res.status != 200 {
		t.Fatalf("/cancel status %d", res.status)
	}
	var out struct {
		Cancelled bool   `json:"cancelled"`
		Op        string `json:"op"`
	}
	if err := json.Unmarshal(res.body, &out); err != nil {
		t.Fatal(err)
	}
	if out.Cancelled || out.Op != "" {
		t.Fatalf("idle project should have nothing to cancel: %+v", out)
	}
	if g := get(t, srv, "/cancel"); g.status != 405 {
		t.Fatalf("GET /cancel should be 405, got %d", g.status)
	}
}
