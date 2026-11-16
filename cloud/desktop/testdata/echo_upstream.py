import json
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        b = json.dumps({"cookie": self.headers.get("Cookie"), "x_amux_member": self.headers.get("X-Amux-Member"),
                        "x_door_user": self.headers.get("X-Door-User"),
                        "x_amux_front_door_owner": self.headers.get("X-Amux-Front-Door-Owner")}).encode()
        self.send_response(200); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
HTTPServer(("127.0.0.1", int(__import__("os").environ["PORT"])), H).serve_forever()
