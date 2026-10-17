import { spawn } from "node:child_process";
import { once } from "node:events";
import { createServer } from "node:http";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, extname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
type Result = { ok: boolean; stdout: string; diagnostics: string; phase: string };
type Options = { compiler: string; timeoutMs?: number; outputLimit?: number };

// Cargo reports the executable path, including any configured target triple.
export async function buildCompiler(): Promise<string> {
    const child = spawn("cargo", ["build", "--locked", "--offline", "--bin", "cog", "--target-dir", join(root, "target", "tutorial"), "--message-format=json"], { cwd: root });
    let output = "";
    child.stdout.setEncoding("utf8").on("data", text => { output += text; });
    child.stderr.pipe(process.stderr);
    const [code] = await once(child, "close");
    if (code !== 0) throw new Error("Cog build failed. Run cargo build --locked to fetch missing dependencies, then retry.");
    for (const line of output.trim().split("\n")) {
        const message = JSON.parse(line);
        if (message.reason === "compiler-artifact" && message.target.name === "cog" && message.executable) {
            await validateCompiler(message.executable);
            return message.executable;
        }
    }
    throw new Error("Cargo did not report the Cog executable.");
}

async function validateCompiler(compiler: string): Promise<void> {
    const child = spawn(compiler, ["--help"]);
    let help = "";
    child.stdout.on("data", chunk => { help += chunk; });
    child.stderr.on("data", chunk => { help += chunk; });
    const [code] = await once(child, "close");
    if (code !== 0 || !help.includes("--restricted")) {
        throw new Error("This Cog executable does not support --restricted. Unset COG_BIN to build the current compiler, then restart the tutorial.");
    }
}

export function createTutorialServer({ compiler, timeoutMs = 5000, outputLimit = 128 * 1024 }: Options) {
    let active = 0;
    const children = new Set<ReturnType<typeof spawn>>();
    const server = createServer(async (request, response) => {
        const json = (status: number, value: unknown) => {
            response.writeHead(status, { "Content-Type": "application/json; charset=utf-8", "Cache-Control": "no-store" });
            response.end(JSON.stringify(value));
        };
        const port = (server.address() as { port: number }).port;
        const hosts = [`127.0.0.1:${port}`, `localhost:${port}`];
        if (!hosts.includes(request.headers.host ?? "")) { json(403, { error: "Invalid host." }); return; }
        const path = new URL(request.url ?? "/", `http://${request.headers.host}`).pathname;
        if (request.method === "GET" && path === "/api/status") { json(200, { ready: true }); return; }
        if (request.method === "POST" && path === "/api/run") {
            if (request.headers.origin !== `http://${request.headers.host}` || request.headers["content-type"] !== "application/json") {
                json(403, { error: "Run examples from the local tutorial page." }); return;
            }
            if (active >= 2) { json(429, { error: "The compiler is busy. Try again shortly." }); return; }
            active += 1;
            const abort = new AbortController();
            response.on("close", () => { if (!response.writableEnded) abort.abort(); });
            let directory: string | undefined;
            try {
                const chunks: Buffer[] = [];
                let size = 0;
                for await (const chunk of request) {
                    size += chunk.length;
                    if (size <= 96 * 1024) chunks.push(chunk);
                }
                if (size > 96 * 1024) { json(413, { error: "Example request is too large." }); return; }
                const payload = JSON.parse(Buffer.concat(chunks).toString("utf8"));
                if (!payload || typeof payload !== "object" || Array.isArray(payload)) { json(400, { error: "Expected an object containing source and input." }); return; }
                const { source, input = "" } = payload;
                if (typeof source !== "string" || !source.trim() || Buffer.byteLength(source) > 64 * 1024 || typeof input !== "string" || Buffer.byteLength(input) > 16 * 1024) {
                    json(400, { error: "Provide source code (up to 64 KiB) and text input (up to 16 KiB)." }); return;
                }
                directory = await mkdtemp(join(tmpdir(), "cog-tutorial-"));
                await writeFile(join(directory, "main.cog"), source);
                const deadline = Date.now() + timeoutMs;
                const execute = (args: string[], stdin: string, phase: string): Promise<Result> => new Promise(resolveResult => {
                    const stdout: Buffer[] = [], stderr: Buffer[] = [];
                    let bytes = 0, stopped = "";
                    const child = spawn(compiler, args, { cwd: directory, signal: abort.signal });
                    children.add(child);
                    const stop = (message: string) => { stopped ||= message; child.kill("SIGKILL"); };
                    const timer = setTimeout(() => stop("Execution time limit reached. Check for an infinite loop."), Math.max(1, deadline - Date.now()));
                    const collect = (chunk: Buffer, output: boolean) => {
                        const remaining = Math.max(0, outputLimit - bytes);
                        bytes += chunk.length;
                        const data = chunk.subarray(0, remaining);
                        (output ? stdout : stderr).push(data);
                        if (bytes > outputLimit) stop("Output limit reached. Print fewer values.");
                    };
                    child.stdout.on("data", chunk => collect(chunk, true));
                    child.stderr.on("data", chunk => collect(chunk, false));
                    child.stdin.on("error", () => {});
                    child.stdin.end(stdin);
                    child.on("error", error => { stopped = error.name === "AbortError" ? "Run cancelled." : "Could not start the Cog compiler."; });
                    child.on("close", code => {
                        clearTimeout(timer);
                        children.delete(child);
                        const diagnostics = Buffer.concat(stderr).toString("utf8").replaceAll(directory + sep, "");
                        resolveResult({ ok: code === 0 && !stopped, stdout: Buffer.concat(stdout).toString("utf8"), diagnostics: stopped || diagnostics, phase });
                    });
                });
                const compiled = await execute(["build", "main.cog", "-o", "program.cogb", "--color", "never"], "", "compile");
                const result = compiled.ok ? await execute(["run", "program.cogb", "--restricted", "--color", "never"], input, "run") : compiled;
                if (!response.destroyed) json(200, result);
            } catch (error) {
                if (!response.destroyed) json(error instanceof SyntaxError ? 400 : 500, { error: error instanceof SyntaxError ? "Invalid JSON request." : "Could not prepare the example." });
            } finally {
                if (directory) await rm(directory, { recursive: true, force: true }).catch(error => console.error("Could not clean up example:", error.message));
                active -= 1;
            }
            return;
        }
        if (request.method !== "GET") { json(405, { error: "Method not allowed." }); return; }
        if (path === "/") { response.writeHead(302, { Location: "/web/get_started.html" }); response.end(); return; }
        try {
            const file = resolve(root, "." + decodeURIComponent(path));
            const publicFile = ["README.md", "ROADMAP.md", "LICENSE", "LICENSE-MPL-2.0", "LICENSE-APACHE-2.0"].some(name => file === join(root, name));
            const document = ["docs", "examples", "web"].some(name => file.startsWith(join(root, name) + sep)) && [".md", ".html", ".cog", ".kdl"].includes(extname(file));
            if (!publicFile && !document) { json(404, { error: "Not found." }); return; }
            const content = await readFile(file);
            response.writeHead(200, { "Content-Type": extname(file) === ".html" ? "text/html; charset=utf-8" : "text/plain; charset=utf-8", "X-Content-Type-Options": "nosniff", "Cache-Control": "no-store" });
            response.end(content);
        } catch { json(404, { error: "Not found." }); }
    });
    server.requestTimeout = 10000;
    server.on("close", () => { for (const child of children) child.kill("SIGKILL"); });
    return server;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
    try {
        const compiler = process.env.COG_BIN ? resolve(process.env.COG_BIN) : await buildCompiler();
        await validateCompiler(compiler);
        const server = createTutorialServer({ compiler });
        server.listen(Number(process.env.PORT ?? 4173), "127.0.0.1");
        await once(server, "listening");
        console.log(`Cog tutorial: http://127.0.0.1:${(server.address() as { port: number }).port}/web/get_started.html`);
        for (const signal of ["SIGINT", "SIGTERM"] as const) process.on(signal, () => server.close());
    } catch (error) { console.error(error instanceof Error ? error.message : error); process.exitCode = 1; }
}
