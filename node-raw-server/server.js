// Raw `http` module Node.js server for the same 2-endpoint TechEmpower-style
// benchmark: GET /json (static, no DB) and GET /db (random row by id out of
// a 10,000-row Postgres table). No framework — bare `http.createServer` plus
// `cluster` for one worker process per core, same fan-out shape as the other
// framework entries in this benchmark (Crystal/Kemal, uvicorn).

const cluster = require('cluster');
const http = require('http');
const os = require('os');
const { Pool } = require('pg');

const PORT = process.env.PORT || 3001;
const DATABASE_URL = process.env.DATABASE_URL || 'postgresql://localhost:5432/node_bench';
const POOL_SIZE = parseInt(process.env.DB_POOL_SIZE || '10', 10);
const WORLD_ROWS = 10000;

if (cluster.isPrimary) {
  const numWorkers = parseInt(process.env.WORKERS || os.cpus().length, 10);
  console.log(`primary ${process.pid} forking ${numWorkers} workers`);
  for (let i = 0; i < numWorkers; i++) cluster.fork();
  cluster.on('exit', (worker, code, signal) => {
    console.error(`worker ${worker.process.pid} died (${signal || code}), restarting`);
    cluster.fork();
  });
} else {
  runWorker();
}

function runWorker() {
  const pool = new Pool({
    connectionString: DATABASE_URL,
    max: POOL_SIZE,
  });

  const JSON_BODY = Buffer.from('{"message":"Hello, World!"}');
  const JSON_HEADERS = {
    'Content-Type': 'application/json',
    'Content-Length': JSON_BODY.length,
  };

  const server = http.createServer((req, res) => {
    if (req.url === '/json') {
      res.writeHead(200, JSON_HEADERS);
      res.end(JSON_BODY);
      return;
    }

    if (req.url === '/db') {
      const id = Math.floor(Math.random() * WORLD_ROWS) + 1;
      pool.query('SELECT id, randomnumber FROM world WHERE id = $1', [id])
        .then((result) => {
          const row = result.rows[0];
          const body = JSON.stringify({ id: row.id, randomNumber: row.randomnumber });
          res.writeHead(200, {
            'Content-Type': 'application/json',
            'Content-Length': Buffer.byteLength(body),
          });
          res.end(body);
        })
        .catch((err) => {
          res.writeHead(500, { 'Content-Type': 'text/plain' });
          res.end('db error: ' + err.message);
        });
      return;
    }

    res.writeHead(404, { 'Content-Type': 'text/plain', 'Content-Length': 9 });
    res.end('Not Found');
  });

  server.keepAliveTimeout = 60000;
  server.listen(PORT, () => {
    console.log(`worker ${process.pid} listening on ${PORT}`);
  });
}
