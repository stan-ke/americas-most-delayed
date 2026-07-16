// Where the dynamic half lives.
//
// These pages are static and free to host (GitHub Pages); everything live comes
// from the server on a VPS, which bills egress. That split is the whole point of
// the deployment, and this file is the seam: the one thing that has to change when
// the API moves.
//
// Must be https — GitHub Pages is served over https, and a browser will refuse to
// call a plain-http API from an https page (mixed content).
window.AMD_API = "<api_url>";

// Working from a local checkout (or file://)? Talk to `cargo run` instead.
if (["localhost", "127.0.0.1", ""].includes(location.hostname)) {
  window.AMD_API = "http://localhost:8080";
}

// The websocket origin is the same host, one scheme over.
window.AMD_WS = window.AMD_API.replace(/^http/, "ws");
