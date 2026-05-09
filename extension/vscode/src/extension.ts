// The module 'vscode' contains the VS Code extensibility API
// Import the module and reference it with the alias vscode in your code below
import * as vscode from "vscode";
import * as cp from "child_process";
import * as fs from "fs";
import * as path from "path";

interface EditContinueOptions {
	session: vscode.DebugSession;
	command: string;
	cwd: string;
	patchPath: string;
	watchGlobs: string[];
	debounceMs: number;
	base?: string | number;
}

interface EditContinueState extends EditContinueOptions {
	watchers: vscode.FileSystemWatcher[];
	timer?: NodeJS.Timeout;
	running: boolean;
	pending: boolean;
	lastReason?: string;
}

let editContinue: EditContinueState | undefined;
let output: vscode.OutputChannel;
const BUGSTALKER_DEBUG_TYPES = new Set(["bugstalker", "lldb"]);
const DEFAULT_DARWIN_TARGET = process.arch === "arm64" ? "aarch64-apple-darwin" : "x86_64-apple-darwin";

// This method is called when your extension is activated
// Your extension is activated the very first time the command is executed
export function activate(context: vscode.ExtensionContext) {
	output = vscode.window.createOutputChannel("BugStalker");
	context.subscriptions.push(output);

	// Use the console to output diagnostic information (console.log) and errors (console.error)
	// This line of code will only be executed once when the extension is activated.
	console.log('Congratulations, your extension "bugstalker" is now active!');

	const disposable = vscode.commands.registerCommand(
		"bugstalker.helloWorld",
		() => {
			vscode.window.showInformationMessage("Hello World from BugStalker!");
		},
	);

	context.subscriptions.push(disposable);
	context.subscriptions.push(
		vscode.commands.registerCommand("bugstalker.applyPatch", async () => {
			await applyPatchFromPicker();
		}),
	);
	context.subscriptions.push(
		vscode.commands.registerCommand("bugstalker.startEditContinue", async () => {
			await startEditContinueFromCommand();
		}),
	);
	context.subscriptions.push(
		vscode.commands.registerCommand("bugstalker.stopEditContinue", () => {
			stopEditContinue();
		}),
	);
	context.subscriptions.push({ dispose: () => stopEditContinue(false) });
	context.subscriptions.push(
		vscode.debug.onDidStartDebugSession(async (session) => {
			if (isBugStalkerSession(session) && editContinueEnabled(session.configuration)) {
				await startEditContinue(session, false);
			}
		}),
	);
	context.subscriptions.push(
		vscode.debug.onDidTerminateDebugSession((session) => {
			if (editContinue?.session.id === session.id) {
				stopEditContinue(false);
			}
		}),
	);

	const adapterFactory = new DebugAdapterExecutableFactory();
	const configProvider = new BugStalkerConfigProvider();
	for (const debugType of BUGSTALKER_DEBUG_TYPES) {
		context.subscriptions.push(vscode.debug.registerDebugAdapterDescriptorFactory(debugType, adapterFactory));
		context.subscriptions.push(vscode.debug.registerDebugConfigurationProvider(debugType, configProvider));
	}
}

async function startEditContinueFromCommand(): Promise<void> {
	const session = vscode.debug.activeDebugSession;
	if (!session || !isBugStalkerSession(session)) {
		vscode.window.showWarningMessage("Start a BugStalker debug session before enabling edit-and-continue.");
		return;
	}
	await startEditContinue(session, true);
}

async function startEditContinue(
	session: vscode.DebugSession,
	allowPrompt: boolean,
): Promise<void> {
	const options = await getEditContinueOptions(session, allowPrompt);
	if (!options) {
		return;
	}
	stopEditContinue(false);

	const watchers = options.watchGlobs.map((glob) => {
		const watcher = vscode.workspace.createFileSystemWatcher(glob);
		const schedule = (uri: vscode.Uri) => scheduleEditContinue(uri.fsPath);
		watcher.onDidCreate(schedule);
		watcher.onDidChange(schedule);
		return watcher;
	});

	editContinue = {
		...options,
		watchers,
		running: false,
		pending: false,
	};
	output.appendLine(
		`[enc] watching ${options.watchGlobs.join(", ")}; command=${options.command}; patch=${options.patchPath}`,
	);
	vscode.window.showInformationMessage("BugStalker edit-and-continue watcher started.");
}

function stopEditContinue(showMessage = true): void {
	if (editContinue?.timer) {
		clearTimeout(editContinue.timer);
	}
	for (const watcher of editContinue?.watchers ?? []) {
		watcher.dispose();
	}
	const hadWatcher = editContinue !== undefined;
	editContinue = undefined;
	if (showMessage && hadWatcher) {
		vscode.window.showInformationMessage("BugStalker edit-and-continue watcher stopped.");
	}
}

function scheduleEditContinue(reason: string): void {
	const state = editContinue;
	if (!state) {
		return;
	}
	if (isGeneratedBuildPath(reason)) {
		output.appendLine(`[enc] ignoring generated build file: ${reason}`);
		return;
	}
	state.lastReason = reason;
	if (state.timer) {
		clearTimeout(state.timer);
	}
	state.timer = setTimeout(() => {
		void runEditContinue();
	}, state.debounceMs);
}

async function runEditContinue(): Promise<void> {
	const state = editContinue;
	if (!state) {
		return;
	}
	if (state.running) {
		state.pending = true;
		return;
	}

	state.running = true;
	try {
		do {
			state.pending = false;
			const reason = state.lastReason ?? "source change";
			output.appendLine(`[enc] ${new Date().toISOString()} ${reason}`);
			output.appendLine(`[enc] running: ${state.command}`);
			vscode.window.setStatusBarMessage("BugStalker: rebuilding edit-and-continue patch...", 2000);

			removeStalePatch(state.patchPath);
			const result = await execShell(state.command, state.cwd);
			if (result.stdout) {
				output.append(result.stdout);
			}
			if (result.stderr) {
				output.append(result.stderr);
			}
			if (result.code !== 0) {
				vscode.window.setStatusBarMessage("BugStalker: edit-and-continue build failed", 4000);
				output.show(true);
				return;
			}
			if (!fs.existsSync(state.patchPath)) {
				output.appendLine(`[enc] no patch emitted at ${state.patchPath}; keeping current debuggee unchanged`);
				const diagnostic = readPatchDiagnostic(state.patchPath);
				if (diagnostic) {
					output.appendLine(`[enc] ${diagnostic}`);
				} else {
					output.appendLine("[enc] no wild --emit-patch diagnostic sidecar found");
				}
				vscode.window.setStatusBarMessage("BugStalker: build succeeded, no patch emitted", 4000);
				return;
			}

			const response = await applyPatch(state.session, state.patchPath, false, state.base, false);
			const entries = response?.entriesApplied ?? 0;
			const bytes = response?.bytesWritten ?? 0;
			const skipped = response?.entriesSkippedDrift ?? 0;
			vscode.window.setStatusBarMessage(
				`BugStalker: patched ${entries} entries, ${bytes} bytes, ${skipped} skipped`,
				4000,
			);
		} while (state.pending);
	} finally {
		state.running = false;
	}
}

function removeStalePatch(patchPath: string): void {
	try {
		fs.unlinkSync(patchPath);
	} catch (err: unknown) {
		if ((err as NodeJS.ErrnoException)?.code !== "ENOENT") {
			output.appendLine(`[enc] failed to remove stale patch ${patchPath}: ${err}`);
		}
	}
	try {
		fs.unlinkSync(patchDiagnosticPath(patchPath));
	} catch (err: unknown) {
		if ((err as NodeJS.ErrnoException)?.code !== "ENOENT") {
			output.appendLine(`[enc] failed to remove stale patch diagnostic ${patchDiagnosticPath(patchPath)}: ${err}`);
		}
	}
}

function isGeneratedBuildPath(filePath: string): boolean {
	return filePath.split(path.sep).includes("target");
}

function readPatchDiagnostic(patchPath: string): string | undefined {
	try {
		const text = fs.readFileSync(patchDiagnosticPath(patchPath), "utf8").trim();
		return text === "" ? undefined : text;
	} catch {
		return undefined;
	}
}

function patchDiagnosticPath(patchPath: string): string {
	return `${patchPath}.log`;
}

async function getEditContinueOptions(
	session: vscode.DebugSession,
	allowPrompt: boolean,
): Promise<EditContinueOptions | undefined> {
	const config = (session.configuration ?? {}) as any;
	const workspaceFolder = session.workspaceFolder?.uri.fsPath ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
	const cwd = expandConfigValue(
		config.editContinueCwd ?? config.cwd ?? workspaceFolder,
		session,
	) ?? workspaceFolder;

	let patchPath = expandConfigValue(config.editContinuePatchPath, session);
	if (!patchPath && cwd) {
		patchPath = `${cwd}/target/bugstalker.wild-patch`;
	}
	let command = expandConfigValue(config.editContinueCommand, session);
	if (!command && cwd && patchPath) {
		command = defaultEditContinueCommand(config, patchPath, cwd);
	}

	if (allowPrompt && !command) {
		command = await vscode.window.showInputBox({
			prompt: "Command to incrementally compile/link and emit a BugStalker wild patch",
			placeHolder: "cargo ... && wild ... --emit-patch=/tmp/bugstalker.patch",
		});
	}
	if (allowPrompt && !patchPath) {
		patchPath = await vscode.window.showInputBox({
			prompt: "Path to the wild patch emitted by your incremental linker",
			value: workspaceFolder ? `${workspaceFolder}/target/bugstalker.wild-patch` : undefined,
		});
	}
	if (!command || !patchPath || !cwd) {
		output.appendLine("[enc] not starting: missing command, patch path, or cwd");
		return undefined;
	}

	return {
		session,
		command,
		cwd,
		patchPath,
		watchGlobs: normalizeWatchGlobs(config.editContinueWatch),
		debounceMs: typeof config.editContinueDebounceMs === "number" ? config.editContinueDebounceMs : 150,
		base: config.editContinueBase,
	};
}

function normalizeWatchGlobs(value: unknown): string[] {
	if (typeof value === "string" && value.trim() !== "") {
		return [value];
	}
	if (Array.isArray(value)) {
		const values = value.filter((item): item is string => typeof item === "string" && item.trim() !== "");
		if (values.length > 0) {
			return values;
		}
	}
	return ["**/*.rs"];
}

function defaultEditContinueCommand(config: any, patchPath: string, cwd?: string): string {
	const cargoArgs = defaultCargoArgs(config);
	const target = config._bugstalkerEditContinueTarget ?? defaultEditContinueTarget();
	const args = ensureCargoTarget(cargoArgs.map((arg: unknown) => String(arg)), target)
		.map((arg: string) => shellQuote(arg))
		.join(" ");
	const rustflags = editContinueRustflags(config, patchPath, cwd);
	const envName = cargoTargetRustflagsEnv(target);
	return `${envName}=${shellQuote(rustflags)} cargo ${args}`;
}

function defaultCargoArgs(config: any): string[] {
	if (Array.isArray(config._bugstalkerCargoArgs) && config._bugstalkerCargoArgs.length > 0) {
		return config._bugstalkerCargoArgs;
	}
	const program = typeof config.program === "string" ? path.basename(config.program) : "";
	if (program && !program.includes("$") && program !== "." && !program.endsWith(".dylib")) {
		return ["build", "--bin", program];
	}
	return ["build"];
}

function defaultEditContinueTarget(): string {
	if (process.platform === "darwin") {
		return DEFAULT_DARWIN_TARGET;
	}
	if (process.platform === "linux" && process.arch === "arm64") {
		return "aarch64-unknown-linux-gnu";
	}
	if (process.platform === "linux" && process.arch === "x64") {
		return "x86_64-unknown-linux-gnu";
	}
	return DEFAULT_DARWIN_TARGET;
}

function cargoTargetRustflagsEnv(target: string): string {
	return `CARGO_TARGET_${target.toUpperCase().replace(/-/g, "_")}_RUSTFLAGS`;
}

function ensureCargoTarget(args: string[], target: string): string[] {
	if (args.some((arg) => arg === "--target" || arg.startsWith("--target="))) {
		return args;
	}
	const separator = args.indexOf("--");
	if (separator >= 0) {
		return [
			...args.slice(0, separator),
			"--target",
			target,
			...args.slice(separator),
		];
	}
	return [...args, "--target", target];
}

function editContinueRustflags(config: any, patchPath: string, cwd?: string): string {
	const linker = resolveEditContinueLinker(config, cwd);
	return [
		"-C",
		"symbol-mangling-version=v0",
		"-C",
		"linker=clang",
		"-C",
		`link-arg=-fuse-ld=${linker}`,
		"-C",
		"link-arg=-Wl,--incremental-cache=read-write",
		"-C",
		`link-arg=-Wl,--emit-patch=${patchPath}`,
	].join(" ");
}

function resolveEditContinueLinker(config: any, cwd?: string): string {
	const configured = config.editContinueLinker ?? config._bugstalkerEditContinueLinker ?? process.env.WILD_LINKER;
	if (typeof configured === "string" && configured.trim() !== "") {
		return configured.split("${cwd}").join(cwd ?? "");
	}
	for (const candidate of wildLinkerCandidates(cwd)) {
		if (fs.existsSync(candidate)) {
			return candidate;
		}
	}
	return "wild";
}

function wildLinkerCandidates(cwd?: string): string[] {
	if (!cwd) {
		return [];
	}
	const candidates: string[] = [];
	let dir = path.resolve(cwd);
	for (;;) {
		candidates.push(path.join(dir, "rec", "linker", "target", "release", "wild"));
		candidates.push(path.join(dir, "rec", "linker", "target", "debug", "wild"));
		candidates.push(path.join(dir, "linker", "target", "release", "wild"));
		candidates.push(path.join(dir, "linker", "target", "debug", "wild"));
		candidates.push(path.join(dir, "linker", "ld"));
		candidates.push(path.join(dir, "wild", "target", "release", "wild"));
		candidates.push(path.join(dir, "wild", "target", "debug", "wild"));
		const parent = path.dirname(dir);
		if (parent === dir) {
			break;
		}
		dir = parent;
	}
	return candidates;
}

function shellQuote(value: string): string {
	if (/^[A-Za-z0-9_/:=.,@%+-]+$/.test(value)) {
		return value;
	}
	return `'${value.replace(/'/g, `'\\''`)}'`;
}

function shellDoubleQuoteContent(value: string): string {
	return value.replace(/["\\$`]/g, (ch) => `\\${ch}`);
}

function expandConfigValue(value: unknown, session: vscode.DebugSession): string | undefined {
	if (typeof value !== "string") {
		return undefined;
	}
	const workspaceFolder = session.workspaceFolder?.uri.fsPath ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? "";
	const cwd = typeof session.configuration?.cwd === "string" ? session.configuration.cwd : workspaceFolder;
	return value
		.split("${workspaceFolder}").join(workspaceFolder)
		.split("${workspaceRoot}").join(workspaceFolder)
		.split("${cwd}").join(cwd);
}

function execShell(command: string, cwd: string): Promise<{ code: number; stdout: string; stderr: string }> {
	return new Promise((resolve) => {
		cp.exec(command, { cwd }, (error, stdout, stderr) => {
			const code =
				typeof (error as cp.ExecException | null)?.code === "number"
					? ((error as cp.ExecException).code as number)
					: error
						? 1
						: 0;
			resolve({ code, stdout, stderr });
		});
	});
}

async function applyPatchFromPicker(): Promise<void> {
	const uri = await pickPatchFile();
	if (!uri) {
		return;
	}
	const session = vscode.debug.activeDebugSession;
	if (!session || !isBugStalkerSession(session)) {
		vscode.window.showWarningMessage("Start a BugStalker debug session before applying a patch.");
		return;
	}
	const response = await applyPatch(session, uri.fsPath, true);
	const entries = response?.entriesApplied ?? 0;
	const bytes = response?.bytesWritten ?? 0;
	const skipped = response?.entriesSkippedDrift ?? 0;
	vscode.window.showInformationMessage(
		`BugStalker applied patch: ${entries} entries, ${bytes} bytes, ${skipped} skipped.`,
	);
}

async function pickPatchFile(): Promise<vscode.Uri | undefined> {
	const picks = await vscode.window.showOpenDialog({
		canSelectFiles: true,
		canSelectFolders: false,
		canSelectMany: false,
		openLabel: "Use Patch",
		filters: {
			"Wild patch": ["patch", "wild-patch", "txt"],
			"All files": ["*"],
		},
	});
	return picks?.[0];
}

async function applyPatch(
	session: vscode.DebugSession,
	patchPath: string,
	showError: boolean,
	base?: string | number,
	verifyExecutableHash = true,
): Promise<any | undefined> {
	try {
		return await session.customRequest("bs/applyPatch", { path: patchPath, base, verifyExecutableHash });
	} catch (err) {
		const message = err instanceof Error ? err.message : String(err);
		output.appendLine(`[enc] patch failed: ${message}`);
		if (showError) {
			vscode.window.showErrorMessage(`BugStalker patch failed: ${message}`);
		} else {
			vscode.window.setStatusBarMessage("BugStalker: patch failed", 4000);
			output.show(true);
		}
		return undefined;
	}
}

class DebugAdapterExecutableFactory
	implements vscode.DebugAdapterDescriptorFactory
{
	createDebugAdapterDescriptor(
		session: vscode.DebugSession,
		executable: vscode.DebugAdapterExecutable | undefined,
	): vscode.ProviderResult<vscode.DebugAdapterDescriptor> {
		console.log("Starting debug adapter", executable);

		const scope = session.workspaceFolder?.uri;
		const settings = vscode.workspace.getConfiguration("bugstalker", scope);
		const bin = settings.get<string>("executable", "bs");
		const args = ["--dap"];
		const logFile = settings.get<string>("logFile");
		if (logFile) {
			args.push("--dap-log-file", logFile);
		}
		const adapterEnv = settings.get<Record<string, string>>("adapterEnv", {});
		const env = { ...process.env, ...adapterEnv };

		if (!env["RUST_LOG"]) {
			env["RUST_LOG"] = "info,bugstalker=info";
		}

		return new vscode.DebugAdapterExecutable(
			bin,
			args,
			{
				cwd: session.configuration?.cwd ?? session.workspaceFolder?.uri.fsPath,
				env,
			},
		);
	}
}

class BugStalkerConfigProvider implements vscode.DebugConfigurationProvider {
	async resolveDebugConfiguration(
		folder: vscode.WorkspaceFolder | undefined,
		config: vscode.DebugConfiguration,
	): Promise<vscode.DebugConfiguration | null> {
		if (config.type === undefined) {
			config.type = "lldb";
		}
		if (typeof config.args === "string") {
			config.args = splitArgs(config.args);
		}
		if (config.cargo) {
			const cargoCwd = expandConfigString(
				config.cargo.cwd ?? config.cwd ?? folder?.uri.fsPath ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
				folder,
				config,
			);
			if (!cargoCwd) {
				vscode.window.showErrorMessage("BugStalker cargo launch needs a workspace folder or cwd.");
				return null;
			}
			const settings = vscode.workspace.getConfiguration("bugstalker", folder?.uri);
			const adapterEnv = settings.get<Record<string, string>>("adapterEnv", {});
			const originalCargoArgs = Array.isArray(config.cargo.args) && config.cargo.args.length > 0
				? [...config.cargo.args]
				: ["build"];
			let cargo = config.cargo;
			let cargoEnv = adapterEnv;
			if (editContinueEnabled(config) && !config.editContinueCommand) {
				const patchPath = expandConfigString(
					config.editContinuePatchPath ?? `${cargoCwd}/target/bugstalker.wild-patch`,
					folder,
					config,
				) ?? `${cargoCwd}/target/bugstalker.wild-patch`;
				const target = config.editContinueTarget ?? defaultEditContinueTarget();
				const linker = resolveEditContinueLinker(config, cargoCwd);
				const cargoArgs = ensureCargoTarget(originalCargoArgs.map((arg: unknown) => String(arg)), target);
				const encConfig = {
					...config,
					_bugstalkerEditContinueLinker: linker,
					_bugstalkerEditContinueTarget: target,
				};
				cargo = { ...config.cargo, args: cargoArgs };
				cargoEnv = {
					...adapterEnv,
					[cargoTargetRustflagsEnv(target)]: editContinueRustflags(encConfig, patchPath, cargoCwd),
				};
				config.editContinuePatchPath = patchPath;
				config._bugstalkerCargoArgs = cargoArgs;
				config._bugstalkerEditContinueLinker = linker;
				config._bugstalkerEditContinueTarget = target;
			} else {
				config._bugstalkerCargoArgs = originalCargoArgs;
			}
			const program = await getProgramFromCargo(cargo, cargoCwd, cargoEnv);
			if (!program) {
				return null;
			}
			config._bugstalkerCargoCwd = cargoCwd;
			config.program = typeof config.program === "string"
				? config.program.split("${cargo:program}").join(program)
				: program;
			delete config.cargo;
		} else if (editContinueEnabled(config) && !config.editContinueCommand && isRustAnalyzerTempProgram(config.program)) {
			const cargoCwd = expandConfigString(
				config.cwd ?? folder?.uri.fsPath ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
				folder,
				config,
			);
			if (!cargoCwd) {
				vscode.window.showErrorMessage("BugStalker Rust Analyzer launch needs a workspace folder or cwd.");
				return null;
			}
			const binName = path.basename(config.program);
			const patchPath = expandConfigString(
				config.editContinuePatchPath ?? `${cargoCwd}/target/bugstalker.wild-patch`,
				folder,
				config,
			) ?? `${cargoCwd}/target/bugstalker.wild-patch`;
			const target = config.editContinueTarget ?? defaultEditContinueTarget();
			const linker = resolveEditContinueLinker(config, cargoCwd);
			const cargoArgs = ensureCargoTarget(["build", "--bin", binName], target);
			const encConfig = {
				...config,
				_bugstalkerEditContinueLinker: linker,
				_bugstalkerEditContinueTarget: target,
			};
			const settings = vscode.workspace.getConfiguration("bugstalker", folder?.uri);
			const adapterEnv = settings.get<Record<string, string>>("adapterEnv", {});
			const cargoEnv = {
				...adapterEnv,
				[cargoTargetRustflagsEnv(target)]: editContinueRustflags(encConfig, patchPath, cargoCwd),
			};
			const program = await getProgramFromCargo(
				{ args: cargoArgs, filter: { name: binName, kind: "bin" } },
				cargoCwd,
				cargoEnv,
			);
			if (!program) {
				return null;
			}
			config.program = program;
			config.editContinuePatchPath = patchPath;
			config._bugstalkerCargoArgs = cargoArgs;
			config._bugstalkerCargoCwd = cargoCwd;
			config._bugstalkerEditContinueLinker = linker;
			config._bugstalkerEditContinueTarget = target;
		}
		return config;
	}
}

function isRustAnalyzerTempProgram(program: unknown): program is string {
	if (typeof program !== "string") {
		return false;
	}
	const normalized = path.normalize(program);
	return normalized.includes(`${path.sep}ra${path.sep}debug${path.sep}`);
}

function editContinueEnabled(config: any): boolean {
	return config?.editContinue === true;
}

async function getProgramFromCargo(
	cargo: any,
	cwd: string,
	extraEnv: Record<string, string>,
): Promise<string | undefined> {
	const args = Array.isArray(cargo.args) ? [...cargo.args] : ["build"];
	const separator = args.indexOf("--");
	args.splice(separator >= 0 ? separator : args.length, 0, "--message-format=json");
	output.appendLine(`[cargo] cargo ${args.join(" ")}`);

	const artifacts: Array<{ fileName: string; name: string; kind: string }> = [];
	const code = await new Promise<number>((resolve) => {
		const child = cp.spawn("cargo", args, {
			cwd,
			env: { ...process.env, ...extraEnv },
			stdio: ["ignore", "pipe", "pipe"],
		});
		let stdout = "";
		child.stdout.on("data", (chunk) => {
			stdout += chunk.toString();
			let newline = stdout.indexOf("\n");
			while (newline >= 0) {
				const line = stdout.slice(0, newline);
				stdout = stdout.slice(newline + 1);
				readCargoLine(line, artifacts);
				newline = stdout.indexOf("\n");
			}
		});
		child.stderr.on("data", (chunk) => output.append(chunk.toString()));
		child.on("error", (err) => {
			output.appendLine(`[cargo] failed to launch cargo: ${err.message}`);
			resolve(1);
		});
		child.on("exit", (exitCode) => resolve(exitCode ?? 1));
	});
	if (code !== 0) {
		output.show(true);
		vscode.window.showErrorMessage(`BugStalker cargo build failed with exit code ${code}.`);
		return undefined;
	}

	const filter = cargo.filter ?? {};
	const matching = artifacts.filter((artifact) =>
		(filter.name === undefined || artifact.name === filter.name)
		&& (filter.kind === undefined || artifact.kind === filter.kind)
	);
	if (matching.length !== 1) {
		output.appendLine(`[cargo] matching artifacts: ${JSON.stringify(matching)}`);
		output.show(true);
		vscode.window.showErrorMessage(`BugStalker cargo build produced ${matching.length} matching artifacts.`);
		return undefined;
	}
	return matching[0].fileName;
}

function readCargoLine(
	line: string,
	artifacts: Array<{ fileName: string; name: string; kind: string }>,
): void {
	if (!line.trim()) {
		return;
	}
	try {
		const msg = JSON.parse(line);
		if (msg.reason === "compiler-message" && msg.message?.rendered) {
			output.append(msg.message.rendered);
		}
		if (msg.reason !== "compiler-artifact" || !msg.target) {
			return;
		}
		if (msg.executable) {
			artifacts.push({
				fileName: msg.executable,
				name: msg.target.name,
				kind: Array.isArray(msg.target.kind) ? msg.target.kind[0] : "bin",
			});
		}
	} catch (err) {
		output.appendLine(line);
	}
}

function splitArgs(args: string): string[] {
	return args.match(/(?:[^\s"']+|"[^"]*"|'[^']*')+/g)?.map((arg) =>
		arg.replace(/^(['"])(.*)\1$/, "$2")
	) ?? [];
}

function expandConfigString(
	value: unknown,
	folder: vscode.WorkspaceFolder | undefined,
	config: vscode.DebugConfiguration,
): string | undefined {
	if (typeof value !== "string") {
		return undefined;
	}
	const workspaceFolder = folder?.uri.fsPath ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? "";
	const cwd = typeof config.cwd === "string" ? config.cwd : workspaceFolder;
	return value
		.split("${workspaceFolder}").join(workspaceFolder)
		.split("${workspaceRoot}").join(workspaceFolder)
		.split("${cwd}").join(cwd);
}

function isBugStalkerSession(session: vscode.DebugSession): boolean {
	return BUGSTALKER_DEBUG_TYPES.has(session.type);
}

// This method is called when your extension is deactivated
export function deactivate() {
	stopEditContinue(false);
}
