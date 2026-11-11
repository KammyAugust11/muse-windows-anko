// Tiny stdio MCP server used by the unit tests in src/mcp.rs.
// Exercises: handshake, paginated tools/list, a server→client request (ping),
// non-JSON noise on stdout, tool errors, JSON-RPC errors, and a crash mode.
const readline = require('readline');

if (process.argv.includes('--crash')) {
  process.stderr.write('boom on stderr\n');
  process.exit(1);
}

const send = (msg) => process.stdout.write(JSON.stringify(msg) + '\n');
const tool = (name) => ({
  name,
  description: `${name} tool`,
  inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] },
});

let pingAnswered = false;
process.stdout.write('this line is not JSON and must be ignored\n');

readline.createInterface({ input: process.stdin }).on('line', (line) => {
  const msg = JSON.parse(line);
  if (msg.id === 'srv-ping') {
    pingAnswered = msg.result !== undefined;
    return;
  }
  if (msg.method === undefined || msg.id === undefined) return; // notification
  const ok = (result) => send({ jsonrpc: '2.0', id: msg.id, result });
  const fail = (code, message) => send({ jsonrpc: '2.0', id: msg.id, error: { code, message } });

  switch (msg.method) {
    case 'initialize':
      return ok({
        protocolVersion: msg.params.protocolVersion,
        capabilities: { tools: {} },
        serverInfo: { name: 'mock', version: '0.0.1' },
      });
    case 'tools/list':
      if (!msg.params || !msg.params.cursor) {
        send({ jsonrpc: '2.0', id: 'srv-ping', method: 'ping' });
        return ok({ tools: [tool('echo')], nextCursor: 'page2' });
      }
      // The second page is only served once the client answered our ping.
      return pingAnswered ? ok({ tools: [tool('fail')] }) : fail(-32000, 'client did not answer ping');
    case 'tools/call':
      if (msg.params.name === 'echo') {
        return ok({ content: [{ type: 'text', text: msg.params.arguments.text }] });
      }
      if (msg.params.name === 'fail') {
        return ok({ content: [{ type: 'text', text: 'it failed' }], isError: true });
      }
      return fail(-32602, `Unknown tool: ${msg.params.name}`);
    default:
      return fail(-32601, 'Method not found');
  }
});
