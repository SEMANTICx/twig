import json, os
from http.server import BaseHTTPRequestHandler, HTTPServer

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/":
            body = json.dumps({"service": "inventory"}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_response(404)
            self.end_headers()

    def log_message(self, *args):
        print("request", *args, flush=True)

if __name__ == "__main__":
    port = int(os.environ.get("PORT", "8000"))
    print(f"serving on {port}", flush=True)
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
