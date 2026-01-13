from http.server import HTTPServer, BaseHTTPRequestHandler
class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")
if __name__ == '__main__':
    server = HTTPServer(('localhost', 8891), Handler)
    server.serve_forever()
