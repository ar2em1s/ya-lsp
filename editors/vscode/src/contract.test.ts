/**
 * The settings this extension sends, against the real server.
 *
 * The server deserializes `initializationOptions` with serde's `deny_unknown_fields`, which does
 * not degrade: one misspelled key rejects the *entire* layer, so every other setting silently
 * stops working and the only evidence is one `window/showMessage` nobody reads. A type checker
 * cannot see across that boundary and a Rust test cannot see this side of it, so the contract is
 * checked by talking to the actual binary.
 *
 * Skipped when there is no binary to talk to, so `npm test` still works on its own.
 */

import assert from 'node:assert/strict';
import { ChildProcessWithoutNullStreams, spawn } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { test } from 'node:test';

import { Settings, serverOptions } from './config';

const binary = findBinary();

function findBinary(): string | undefined {
  const root = path.resolve(__dirname, '..', '..', '..');
  const candidates = [
    path.resolve(__dirname, '..', 'server', 'ya-lsp'),
    path.join(root, 'target', 'release', 'ya-lsp'),
    path.join(root, 'target', 'debug', 'ya-lsp'),
  ];
  return candidates.find((candidate) => fs.existsSync(candidate));
}

/** Every setting the extension knows how to send, all set at once. */
const everything: Settings = {
  explicit<T>(key: string): T | undefined {
    const values: Record<string, unknown> = {
      'index.maxFiles': 4321,
      'gems.enabled': false,
      'gems.defaultGems': false,
      'gems.rubyVersion': '3.3.0',
      'rbs.enabled': true,
      'rbs.stdlib': false,
      'rbs.path': '/opt/rbs',
      'diagnostics.enabled': true,
      'diagnostics.rules': { 'parse-error': 'error' },
    };
    return values[key] as T | undefined;
  },
};

test('the server accepts every setting the extension can send', { skip: !binary }, async () => {
  const complaints = await initialize(serverOptions(everything));
  assert.deepEqual(complaints, [], 'the server rejected settings this extension sends');
});

test('and would have said so about one it could not read', { skip: !binary }, async () => {
  // The guard on the test above: without it, a change that broke every key would still pass by
  // producing no complaint about keys the server never saw. `rubyVersion` is the camelCase this
  // extension would write if nobody had checked.
  const complaints = await initialize({ gems: { rubyVersion: '3.3.0' } });
  assert.equal(complaints.length, 1, 'a camelCase key has to be rejected, not ignored');
  assert.match(complaints[0], /unknown field/);
});

/** Start the server, hand it `options`, and collect what it says about them. */
async function initialize(options: unknown): Promise<string[]> {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'ya-lsp-contract-'));
  // A workspace with nothing in it is itself worth a warning, and this test is not about that.
  fs.writeFileSync(path.join(root, 'main.rb'), 'class Main\nend\n');
  const server = spawn(binary!, ['--stdio'], { cwd: root });
  const messages: string[] = [];

  try {
    const reader = read(server);
    send(server, {
      jsonrpc: '2.0',
      id: 1,
      method: 'initialize',
      params: {
        processId: process.pid,
        rootUri: `file://${root}`,
        capabilities: {},
        initializationOptions: options,
      },
    });

    // The handshake has to be finished, not just answered: the server reports config problems
    // after `initialize_finish`, which blocks until the client says `initialized`.
    await reader.until((message) => message.id === 1);
    send(server, { jsonrpc: '2.0', method: 'initialized', params: {} });
    for (const message of await reader.drain(400)) {
      // Only what the server says about the settings: a machine with no Ruby installed has
      // other things to report, and none of them are this test's business.
      const text = String(message.params?.message ?? '');
      if (message.method === 'window/showMessage' && text.includes('initializationOptions')) {
        messages.push(text);
      }
    }
  } finally {
    server.kill();
    fs.rmSync(root, { recursive: true, force: true });
  }
  return messages;
}

interface Incoming {
  id?: number;
  method?: string;
  params?: { message?: unknown } & Record<string, unknown>;
}

function send(server: ChildProcessWithoutNullStreams, message: unknown): void {
  const body = Buffer.from(JSON.stringify(message), 'utf8');
  server.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
  server.stdin.write(body);
}

/** The smallest LSP framing reader that can be trusted: length-prefixed, never line-based. */
function read(server: ChildProcessWithoutNullStreams) {
  let buffer = Buffer.alloc(0);
  const queue: Incoming[] = [];
  const waiters: (() => void)[] = [];

  server.stdout.on('data', (chunk: Buffer) => {
    buffer = Buffer.concat([buffer, chunk]);
    for (;;) {
      const header = buffer.indexOf('\r\n\r\n');
      if (header < 0) {
        break;
      }
      const length = Number(/Content-Length: (\d+)/i.exec(buffer.subarray(0, header).toString())?.[1]);
      const start = header + 4;
      if (!Number.isFinite(length) || buffer.length < start + length) {
        break;
      }
      queue.push(JSON.parse(buffer.subarray(start, start + length).toString('utf8')) as Incoming);
      buffer = buffer.subarray(start + length);
      waiters.splice(0).forEach((resolve) => resolve());
    }
  });

  const next = async (): Promise<Incoming | undefined> => {
    if (queue.length === 0) {
      await new Promise<void>((resolve) => {
        waiters.push(resolve);
        // A server that stops talking must fail the test, not hang it.
        setTimeout(resolve, 5000);
      });
    }
    return queue.shift();
  };

  return {
    async until(matches: (message: Incoming) => boolean): Promise<void> {
      for (;;) {
        const message = await next();
        if (!message) {
          throw new Error('the server stopped talking before answering');
        }
        if (matches(message)) {
          return;
        }
      }
    },
    async drain(milliseconds: number): Promise<Incoming[]> {
      await new Promise((resolve) => setTimeout(resolve, milliseconds));
      return queue.splice(0);
    },
  };
}
