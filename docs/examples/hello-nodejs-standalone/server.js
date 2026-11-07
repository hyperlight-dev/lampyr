const app = require('./application');

const server = app.listen(Number(process.env.PORT ?? 8080), process.env.LAMPYR_LISTEN_HOST || '0.0.0.0');
for (const signal of ['SIGINT', 'SIGTERM']) {
    process.on(signal, () => server.close());
}
