import assert from "node:assert/strict";
import { before, after, test } from "node:test";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { readFile, access } from "node:fs/promises";
import { buildCompiler, createTutorialServer } from "./server.mts";

let server: ReturnType<typeof createTutorialServer>;
let origin: string;
before(async () => {
    server = createTutorialServer({ compiler: await buildCompiler(), timeoutMs: 1500, outputLimit: 8192 });
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    origin = `http://127.0.0.1:${(server.address() as { port: number }).port}`;
});
after(async () => { if (server) await new Promise<void>(done => { server.closeAllConnections(); server.close(() => done()); }); });

async function run(source: string, input = "") {
    const response = await fetch(origin + "/api/run", { method: "POST", headers: { Origin: origin, "Content-Type": "application/json" }, body: JSON.stringify({ source, input }) });
    assert.equal(response.status, 200);
    return response.json();
}

test("all thirteen tutorial programs compile and execute through real bytecode", async () => {
    const html = await readFile(new URL("./get_started.html", import.meta.url), "utf8");
    const sources = [...html.matchAll(/<textarea class="source"[^>]*>([\s\S]*?)<\/textarea>/g)].map(match => match[1].replaceAll("&lt;", "<").replaceAll("&gt;", ">").replaceAll("&quot;", '"').replaceAll("&#x27;", "'").replaceAll("&amp;", "&"));
    const outputs = ["Hello, Cog!\n", "Cog: 3\n", "42\n", "Cog: 1\nCog: 3\n", "2\n1\nsum=10, large\n", "[0, 9, 2, 3]\n4\n", "Plain: {name}\nHello, Ada! 2 + 3 = 5\n{Cog}\n", "Move to 2\nmoving\n", "3\n7\nParseFailure\nDone.\n", "42\nCog\n3\n", "right operand ran\nthe left Error survived the operation\n7\nthe selected array element is an Error\n", "Cog tools\nred / green / blue\n3\n", "42\n"];
    assert.equal(sources.length, outputs.length);
    for (let i = 0; i < sources.length; ++i) {
        const result = await run(sources[i], " Ada \n");
        assert.equal(result.ok, true, result.diagnostics);
        assert.equal(result.stdout, outputs[i]);
        assert.equal(result.diagnostics, "");
    }
});

test("syntax and type errors return compiler diagnostics without executing", async () => {
    for (const source of ['fn main() { const x = ; }', 'include std; use io; fn main() { io::println("should not run"); const n: int = "wrong"; }']) {
        const result = await run(source);
        assert.equal(result.ok, false);
        assert.equal(result.phase, "compile");
        assert.equal(result.stdout, "");
        assert.match(result.diagnostics, /error\[E\d+\]/);
        assert.match(result.diagnostics, /main\.cog:/);
    }
});

test("runtime failure preserves earlier output and stdin reaches read_line", async () => {
    const failed = await run('include std; use io; fn main() { io::println("before"); (1 / 0)!!; }');
    assert.equal(failed.ok, false);
    assert.equal(failed.phase, "run");
    assert.equal(failed.stdout, "before\n");
    assert.match(failed.diagnostics, /division by zero/);
    const read = await run('include std; use io; fn main() { io::println(io::read_line()!!)!!; }', "café λ\n");
    assert.equal(read.ok, true, read.diagnostics);
    assert.equal(read.stdout, "café λ\n");
    const eof = await run('include std; use io; fn main() { io::read_line()!!; }');
    assert.equal(eof.ok, false);
});

test("runtime host effects are denied before touching files or starting processes", async () => {
    const path = new URL("./tutorial-must-not-create.txt", import.meta.url).pathname;
    for (const source of [
        `include std; use io; fn main() { io::open(${JSON.stringify(path)}, "w", 0)!!; }`,
        'include std; use process; fn main() { const args: str[] = []; process::run("unused", args)!!; }',
        'include std; use time; fn main() { time::now()!!; }',
        'include std; use entropy; fn main() { entropy::bytes(1)!!; }',
    ]) {
        const result = await run(source);
        assert.equal(result.ok, false);
        assert.match(result.diagnostics, /permission denied/i);
    }
    await assert.rejects(access(path));
});

test("infinite loops and excess output stop and subsequent runs still work", async () => {
    const timeout = await run("fn main() { while true {} }");
    assert.equal(timeout.ok, false);
    assert.match(timeout.diagnostics, /time limit/);
    const output = await run('include std; use io; fn main() { while true { io::println("too much output"); } }');
    assert.equal(output.ok, false);
    assert.match(output.diagnostics, /Output limit/);
    const next = await run('include std; use io; fn main() { io::println("ready"); }');
    assert.equal(next.stdout, "ready\n");
    assert.equal(next.ok, true);
});

test("HTTP serves the tutorial, rejects foreign origins, and confines static files", async () => {
    const page = await fetch(origin + "/web/get_started.html");
    assert.equal(page.status, 200);
    assert.match(await page.text(), /Get started with Cog/);
    assert.equal((await fetch(origin + "/api/status")).status, 200);
    assert.equal((await fetch(origin + "/src/main.rs")).status, 404);
    assert.equal((await fetch(origin + "/web/%2e%2e%2fsrc/main.rs")).status, 404);
    const foreign = await fetch(origin + "/api/run", { method: "POST", headers: { Origin: "https://example.invalid", "Content-Type": "application/json" }, body: '{}' });
    assert.equal(foreign.status, 403);
    const invalid = await fetch(origin + "/api/run", { method: "POST", headers: { Origin: origin, "Content-Type": "application/json" }, body: 'invalid' });
    assert.equal(invalid.status, 400);
    const empty = await fetch(origin + "/api/run", { method: "POST", headers: { Origin: origin, "Content-Type": "application/json" }, body: '{}' });
    assert.equal(empty.status, 400);
});

test("invalid payloads and oversized requests return useful HTTP errors", async () => {
    for (const body of ['null', '[]', 'true', '{"source":4}', '{"source":"fn main() {}","input":{}}']) {
        const response = await fetch(origin + "/api/run", { method: "POST", headers: { Origin: origin, "Content-Type": "application/json" }, body });
        assert.equal(response.status, 400);
    }
    const response = await fetch(origin + "/api/run", { method: "POST", headers: { Origin: origin, "Content-Type": "application/json" }, body: JSON.stringify({source:"x".repeat(100 * 1024)}) });
    assert.equal(response.status, 413);
});

test("a stale configured executable is rejected before the server starts", async () => {
    const child = spawn(process.execPath, [new URL("./server.mts", import.meta.url).pathname], {env:{...process.env,COG_BIN:process.execPath,PORT:"0"}});
    let stderr = "";
    child.stderr.on("data", chunk => {stderr += chunk;});
    const [code] = await once(child,"close");
    assert.equal(code,1);
    assert.match(stderr,/does not support --restricted/);
});
