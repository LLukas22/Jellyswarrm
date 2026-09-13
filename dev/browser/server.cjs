const { chromium } = require('playwright');

(async () => {
    const server = await chromium.launchServer({
        channel: 'chrome',
        headless: true,
        host: '127.0.0.1',
        port: Number(process.env.PLAYWRIGHT_PORT),
        wsPath: process.env.PLAYWRIGHT_WS_PATH,
        args: process.env.JELLYSWARRM_BROWSER_ALLOW_AUTOPLAY
            ? ['--autoplay-policy=no-user-gesture-required'] : []
    });
    console.log('Jellyswarrm browser ready');
    const stop = async () => {
        await server.close();
        process.exit(0);
    };
    process.on('SIGTERM', stop);
    process.on('SIGINT', stop);
})().catch(error => {
    console.error(error);
    process.exit(1);
});
