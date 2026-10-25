from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        c = self.headers.get("Cookie", "")
        if "amux_door=member" in c:
            self.send_response(200); self.send_header("X-Door-User", "dana@example.com"); self.send_header("X-Amux-Member", "REAL-TOKEN")
        elif "amux_door=password" in c:
            self.send_response(200); self.send_header("X-Door-User", "ethan")           # no member token
        else:
            self.send_response(401)
        self.send_header("Content-Length", "0"); self.end_headers()
HTTPServer(("127.0.0.1", int(__import__("os").environ["PORT"])), H).serve_forever()
