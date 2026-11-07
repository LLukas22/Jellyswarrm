#!/usr/bin/env node
/* Static discovery, not execution: SDK calls, legacy ApiClient calls and getUrl
 * expressions across the bundled web client. Dynamic URL/dispatch sites remain
 * visible in the output rather than being silently treated as covered. */
const fs = require('node:fs');
const path = require('node:path');
const { createRequire } = require('node:module');
const root = path.resolve(__dirname, '..');
const requireUi = createRequire(path.join(root, 'ui/package.json'));
const ts = requireUi('typescript');
const sdkRoot = path.join(root, 'ui/node_modules/@jellyfin/sdk/lib/generated-client/api');
const output = path.join(root, 'crates/jellyswarrm-proxy/src/http_tests/client_routes.json');
function walk(node, visit) { visit(node); ts.forEachChild(node, child => walk(child, visit)); }
function parse(name, text) { return ts.createSourceFile(name, text, ts.ScriptTarget.Latest, true, name.endsWith('.tsx') ? ts.ScriptKind.TSX : name.endsWith('.js') ? ts.ScriptKind.JSX : ts.ScriptKind.TS); }
function literal(node) { return node && (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) ? node.text : undefined; }
function template(node) {
    if (!node) return null;
    const value = literal(node);
    if (value !== undefined) return value;
    if (ts.isTemplateExpression(node)) return node.head.text + node.templateSpans.map(span => `{${span.expression.getText()}}${span.literal.text}`).join('');
    if (ts.isBinaryExpression(node) && node.operatorToken.kind === ts.SyntaxKind.PlusToken) return template(node.left) + template(node.right);
    if (ts.isCallExpression(node) && node.expression.getText() === 'encodeURIComponent') return `{${node.arguments[0].getText()}}`;
    return `{${node.getText()}}`;
}
const sdk = new Map();
for (const file of fs.readdirSync(sdkRoot).filter(file => file.endsWith('.js'))) {
    const factory = 'get' + file.replace(/\.js$/, '').split('-').map(part => part[0].toUpperCase()+part.slice(1)).join('');
    const ast = parse(file, fs.readFileSync(path.join(sdkRoot,file),'utf8'));
    walk(ast, node => {
        if (!ts.isPropertyAssignment(node) || !ts.isArrowFunction(node.initializer)) return;
        let route, method, body = false; const query = [];
        walk(node.initializer, child => {
            if (ts.isVariableDeclaration(child) && child.name.getText() === 'localVarPath') {
                walk(child.initializer, part => { if (route === undefined && literal(part) !== undefined) route = literal(part); });
            }
            if (ts.isPropertyAssignment(child) && child.name.getText() === 'method') method = literal(child.initializer);
            if (ts.isElementAccessExpression(child) && child.expression.getText() === 'localVarQueryParameter') query.push(literal(child.argumentExpression));
            if (ts.isPropertyAccessExpression(child) && child.getText() === 'localVarRequestOptions.data') body = true;
        });
        if (route && method) sdk.set(`${factory}.${node.name.getText()}`, { method, path: route, query: [...new Set(query)].sort(), body });
    });
}
const map = JSON.parse(fs.readFileSync(path.join(root,'ui/node_modules/jellyfin-apiclient/dist/jellyfin-apiclient.js.map'),'utf8'));
const legacySource = map.sourcesContent[map.sources.findIndex(name => name.endsWith('/src/apiClient.js'))];
if (!legacySource) throw new Error('Legacy ApiClient source map is required to resolve wrapper methods');
const legacy = new Map();
walk(parse('apiClient.js',legacySource), node => {
    if (!ts.isMethodDeclaration(node)) return;
    const routes = [], delegates = [], methods = new Set();
    walk(node, child => {
        if (ts.isCallExpression(child) && ts.isPropertyAccessExpression(child.expression)) {
            const name = child.expression.name.text;
            if (name === 'getUrl') routes.push(template(child.arguments[0]));
            else if (name === 'getJSON') methods.add('GET');
            else if (child.expression.expression.getText() === 'this') delegates.push(name);
        }
        if (/^(GET|POST|DELETE|PUT|HEAD|PATCH)$/.test(literal(child)||'')) methods.add(literal(child));
    });
    legacy.set(node.name.getText(), { routes, delegates, methods: [...methods] });
});
function legacyRoutes(name, seen = new Set()) {
    if (seen.has(name)) return [];
    seen.add(name);
    const entry = legacy.get(name);
    if (!entry) return [];
    if (entry.routes.length) return entry.routes.flatMap(route => (entry.methods.length ? entry.methods : ['GET']).map(method => ({method,path:route})));
    return entry.delegates.flatMap(name => legacyRoutes(name,seen));
}
const inScope = route => /^\/?(Items|UserItems|UserViews|UserPlayedItems|UserFavoriteItems|Users\/.*\/(Items|Views|PlayedItems|FavoriteItems)|Library\/MediaFolders|Shows|Movies|Genres|MusicGenres|Studios|Persons|Artists|Search|Playlists|Collections|Videos|Audio|LiveTv|LiveStreams|MediaSegments|DisplayPreferences|Sessions\/Playing|Playback|FallbackFont)(\/|$)/i.test(route);
const entries = [], dynamic = [];
function files(dir) { return fs.readdirSync(dir,{withFileTypes:true}).flatMap(entry => entry.isDirectory() ? files(path.join(dir,entry.name)) : [path.join(dir,entry.name)]); }
for (const file of files(path.join(root,'ui/src')).filter(file => /\.(js|ts|tsx)$/.test(file) && !/\.(test|spec)\.[jt]sx?$|\.d\.ts$/.test(file))) {
    const ast = parse(file,fs.readFileSync(file,'utf8'));
    const aliases = new Map();
    walk(ast,node => {
        if (ts.isVariableDeclaration(node) && node.initializer && ts.isCallExpression(node.initializer) && /^get\w+Api$/.test(node.initializer.expression.getText())) aliases.set(node.name.getText(),node.initializer.expression.getText());
    });
    const site = node => ({file:path.relative(root,file),line:ast.getLineAndCharacterOfPosition(node.getStart()).line+1});
    walk(ast,node => {
        if (!ts.isCallExpression(node)) return;
        if (ts.isElementAccessExpression(node.expression) && /apiClient/i.test(node.expression.expression.getText())) {
            dynamic.push({...site(node),expression:node.expression.getText(),reason:'dynamic method dispatch'});
        }
        if (!ts.isPropertyAccessExpression(node.expression)) return;
        const receiver = node.expression.expression;
        const name = node.expression.name.text;
        const factory = ts.isCallExpression(receiver) ? receiver.expression.getText() : aliases.get(receiver.getText());
        const helper = name === 'getItemImageUrlById' ? 'getItemImageByIndex' : name.replace(/Url$/, '');
        const resolved = sdk.get(`${factory}.${name}`) || sdk.get(`${factory}.${helper}`);
        if (resolved && inScope(resolved.path)) entries.push({...resolved,operation:`${factory}.${name}`,source:'sdk',...site(node)});
        if (/apiClient/i.test(receiver.getText())) {
            if (name === 'getUrl') {
                const route = template(node.arguments[0]);
                if (route && inScope(route)) entries.push({method:'CONTEXT',path:route,operation:'getUrl',source:'literal',...site(node)});
                else if (route?.startsWith('{')) dynamic.push({...site(node),expression:node.getText(),reason:'computed or server-supplied URL'});
            } else {
                for (const route of legacyRoutes(name)) {
                    if (route.path && inScope(route.path)) entries.push({...route,operation:name,source:'legacy',...site(node)});
                    else if (route.path?.startsWith('{')) dynamic.push({...site(node),expression:node.getText(),reason:`legacy ${name}: computed URL`});
                }
            }
        }
    });
}
const sorted = values => values.sort((a,b) => a.file.localeCompare(b.file) || a.line-b.line || (a.path||a.expression).localeCompare(b.path||b.expression));
const result = JSON.stringify({schema:1,description:'Static client call sites. CONTEXT methods and dynamic sites require manual contract review; this is not a runtime coverage claim.',requests:sorted(entries),dynamic:sorted(dynamic)},null,2)+'\n';
if (process.argv.includes('--check')) {
    if (!fs.existsSync(output) || fs.readFileSync(output,'utf8') !== result) { console.error('Client route inventory is stale. Run node scripts/scan-client-routes.cjs --write'); process.exit(1); }
} else if (process.argv.includes('--write')) fs.writeFileSync(output,result);
else process.stdout.write(result);
console.error(`${entries.length} resolved call sites; ${dynamic.length} dynamic sites requiring manual coverage`);
