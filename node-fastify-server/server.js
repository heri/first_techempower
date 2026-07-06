// Fastify implementation of the same 2-endpoint TechEmpower-style benchmark:
// GET /json (static, no DB) and GET /db (random row by id out of a
// 10,000-row Postgres table). Idiomatic Fastify: response JSON schemas (so
// Fastify uses its fast-json-stringify serializer instead of JSON.stringify),
// `cluster` for one worker process per core, same fan-out shape as the other
// framework entries in this benchmark (Crystal/Kemal, uvicorn, node-raw).

const cluster = require('cluster');
const os = require('os');
const { Pool } = require('pg');

const PORT = process.env.PORT || 3002;
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

async function runWorker() {
  const fastify = require('fastify')({ logger: false });
  const pool = new Pool({
    connectionString: DATABASE_URL,
    max: POOL_SIZE,
  });

  fastify.get('/json', {
    schema: {
      response: {
        200: {
          type: 'object',
          properties: { message: { type: 'string' } },
        },
      },
    },
  }, async () => {
    return { message: 'Hello, World!' };
  });

  fastify.get('/db', {
    schema: {
      response: {
        200: {
          type: 'object',
          properties: {
            id: { type: 'integer' },
            randomNumber: { type: 'integer' },
          },
        },
      },
    },
  }, async () => {
    const id = Math.floor(Math.random() * WORLD_ROWS) + 1;
    const result = await pool.query('SELECT id, randomnumber FROM world WHERE id = $1', [id]);
    const row = result.rows[0];
    return { id: row.id, randomNumber: row.randomnumber };
  });

  try {
    await fastify.listen({ port: PORT, host: '0.0.0.0' });
    console.log(`worker ${process.pid} listening on ${PORT}`);
  } catch (err) {
    fastify.log.error(err);
    process.exit(1);
  }
}
