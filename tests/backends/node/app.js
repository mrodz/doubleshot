const express = require('express');
const app = express();
const port = process.env.PORT || 3000;
const version = process.env.VERSION || 'blue';
const startupDelay = parseInt(process.env.STARTUP_DELAY_MS || '8000', 10);

let ready = false;

app.get('/health', (req, res) => {
  if (!ready) {
    return res.status(503).json({ status: 'starting', version });
  }
  res.json({ status: 'ok', version });
});

app.get('/', (req, res) => {
  res.json({ message: 'Hello from DoubleShot!', version });
});

const server = app.listen(port, () => {
  console.log(`[${version}] Listening on :${port} — warming up for ${startupDelay}ms...`);

  setTimeout(() => {
    ready = true;
    console.log(`[${version}] Ready.`);
  }, startupDelay);
});

// Graceful shutdown: stop accepting new connections, finish in-flight requests.
function shutdown(signal) {
  console.log(`[${version}] ${signal} received — shutting down gracefully`);
  server.close(() => {
    console.log(`[${version}] All connections drained. Exiting.`);
    process.exit(0);
  });
}

process.on('SIGTERM', () => shutdown('SIGTERM'));
process.on('SIGINT',  () => shutdown('SIGINT'));