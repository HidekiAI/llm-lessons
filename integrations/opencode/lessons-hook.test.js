// Run: node --test integrations/opencode/lessons-hook.test.js
// Not in the plugins directory on purpose: opencode would load this file as a plugin too.
import { test } from "node:test"
import assert from "node:assert/strict"
import { mkdtempSync, writeFileSync, chmodSync, readFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"

const PLUGIN_URL = new URL("./lessons-hook.js", import.meta.url).href
const SESSION_ID = "ses_test"
const MESSAGE_ID = "msg_test"

const EXECUTABLE_MODE = 0o755
const SHORT_TIMEOUT_MS = "300" // a string: it goes through process.env
const TIMEOUT_TEST_SLEEP_SECONDS = 5
const MS_PER_SECOND = 1000
const OVER_CAP_BYTES = 64 * 1024
const PIPE_OVERFLOW_BYTES = 4 * 1024 * 1024

// A fake `lessons` that records its stdin and prints `stdoutText` (or fails when `exitCode` is not zero). With
// `readsStdin` false it exits without reading, and with `sleepSeconds` it sleeps before answering. The text goes
// through a file, so any character in it is safe.
const fakeLessons = (stdoutText, { exitCode = 0, readsStdin = true, sleepSeconds = 0, ignoresTerm = false } = {}) => {
    const fakeDirectory = mkdtempSync(join(tmpdir(), "fake-lessons-"))
    const stdinRecord = join(fakeDirectory, "stdin.json")
    const stdoutFile = join(fakeDirectory, "stdout.txt")
    const script = join(fakeDirectory, "lessons")
    writeFileSync(stdoutFile, stdoutText)
    const readLine = readsStdin ? `cat > '${stdinRecord}'` : ":"
    const trapLine = ignoresTerm ? "trap '' TERM" : ":"
    const sleepLine = ignoresTerm ? `exec sleep ${sleepSeconds}` : `sleep ${sleepSeconds}`
    writeFileSync(script, `#!/bin/sh\n${trapLine}\n${readLine}\n${sleepLine}\ncat '${stdoutFile}'\nexit ${exitCode}\n`)
    chmodSync(script, EXECUTABLE_MODE)
    return { script, stdinRecord }
}

const runHook = async (lessonsBinary, parts, timeoutMs = "") => {
    const previousBin = process.env.LESSONS_BIN
    const previousTimeout = process.env.LESSONS_HOOK_TIMEOUT_MS
    const restoreEnv = (name, previous) => (previous === undefined ? delete process.env[name] : (process.env[name] = previous))
    process.env.LESSONS_BIN = lessonsBinary
    process.env.LESSONS_HOOK_TIMEOUT_MS = timeoutMs
    try {
        const { LessonsHook } = await import(`${PLUGIN_URL}?bin=${encodeURIComponent(lessonsBinary)}`)
        const hooks = await LessonsHook({ directory: "/work/repo" })
        const output = { message: { id: MESSAGE_ID }, parts }
        await hooks["chat.message"]({ sessionID: SESSION_ID }, output)
        return output.parts
    } finally {
        restoreEnv("LESSONS_BIN", previousBin)
        restoreEnv("LESSONS_HOOK_TIMEOUT_MS", previousTimeout)
    }
}

test("adds one synthetic text part with the lessons output and sends prompt and cwd on stdin", async () => {
    const { script, stdinRecord } = fakeLessons("LESSON: use a different delimiter")
    const parts = await runHook(script, [{ type: "text", text: "perl pipe substitution broke" }])
    assert.equal(parts.length, 2)
    assert.equal(parts[1].type, "text")
    assert.equal(parts[1].text, "LESSON: use a different delimiter")
    assert.equal(parts[1].synthetic, true)
    assert.equal(parts[1].sessionID, SESSION_ID)
    assert.equal(parts[1].messageID, MESSAGE_ID)
    assert.match(parts[1].id, /^prt_/)
    assert.deepEqual(JSON.parse(readFileSync(stdinRecord, "utf8")), {
        prompt: "perl pipe substitution broke",
        cwd: "/work/repo",
    })
})

test("adds nothing when lessons prints nothing", async () => {
    const { script } = fakeLessons("")
    const parts = await runHook(script, [{ type: "text", text: "ok thanks" }])
    assert.equal(parts.length, 1)
})

test("adds nothing and does not throw when lessons fails", async () => {
    const { script } = fakeLessons("partial output", { exitCode: 1 })
    const parts = await runHook(script, [{ type: "text", text: "anything" }])
    assert.equal(parts.length, 1)
})

test("adds nothing and does not throw when the lessons binary is missing", async () => {
    const parts = await runHook("/nonexistent/lessons", [{ type: "text", text: "anything" }])
    assert.equal(parts.length, 1)
})

test("ignores synthetic parts and non-text parts when building the prompt", async () => {
    const { script, stdinRecord } = fakeLessons("L")
    await runHook(script, [
        { type: "text", text: "real prompt" },
        { type: "text", text: "injected earlier", synthetic: true },
        { type: "file", url: "file:///x" },
    ])
    assert.equal(JSON.parse(readFileSync(stdinRecord, "utf8")).prompt, "real prompt")
})

test("does not call lessons for a message with no text", async () => {
    const { script } = fakeLessons("SHOULD NOT APPEAR")
    const parts = await runHook(script, [{ type: "file", url: "file:///x" }])
    assert.equal(parts.length, 1)
})

test("trims the padding around the lessons output", async () => {
    const { script } = fakeLessons("\n  LESSON  \n\n")
    const parts = await runHook(script, [{ type: "text", text: "anything" }])
    assert.equal(parts[1].text, "LESSON")
})

test("output with a quote in it passes through whole", async () => {
    const { script } = fakeLessons("it's a 'quoted' lesson")
    const parts = await runHook(script, [{ type: "text", text: "anything" }])
    assert.equal(parts[1].text, "it's a 'quoted' lesson")
})

test("ignored parts are not part of the prompt", async () => {
    const { script, stdinRecord } = fakeLessons("L")
    await runHook(script, [
        { type: "text", text: "real prompt" },
        { type: "text", text: "not for the model", ignored: true },
    ])
    assert.equal(JSON.parse(readFileSync(stdinRecord, "utf8")).prompt, "real prompt")
})

test("a lessons that hangs is killed at the timeout and adds nothing", async () => {
    const { script } = fakeLessons("LATE", { sleepSeconds: TIMEOUT_TEST_SLEEP_SECONDS })
    const startedAt = Date.now()
    const parts = await runHook(script, [{ type: "text", text: "anything" }], SHORT_TIMEOUT_MS)
    assert.equal(parts.length, 1)
    assert.ok(Date.now() - startedAt < TIMEOUT_TEST_SLEEP_SECONDS * MS_PER_SECOND, "returned before the sleep ended")
})

test("a lessons that exits without reading a large prompt does not crash the host", async () => {
    const { script } = fakeLessons("", { readsStdin: false })
    const parts = await runHook(script, [{ type: "text", text: "x".repeat(PIPE_OVERFLOW_BYTES) }])
    assert.equal(parts.length, 1)
})

test("output past the cap is dropped, not injected", async () => {
    const { script } = fakeLessons("y".repeat(OVER_CAP_BYTES))
    const parts = await runHook(script, [{ type: "text", text: "anything" }])
    assert.equal(parts.length, 1)
})

test("a timeout execFile would refuse or mangle falls back to the default and the hook still works", async () => {
    const { script } = fakeLessons("LESSON")
    const badTimeouts = ["-5", "abc", "0", "Infinity", "0.5", "1e20", "4294967296"]
    const texts = []
    for (const badTimeout of badTimeouts) {
        const parts = await runHook(script, [{ type: "text", text: "anything" }], badTimeout)
        texts.push(parts[1]?.text)
    }
    assert.deepEqual(texts, badTimeouts.map(() => "LESSON"))
})

test("a fractional timeout is cut to whole milliseconds and does not make the hook throw", async () => {
    const { script } = fakeLessons("LESSON")
    const parts = await runHook(script, [{ type: "text", text: "anything" }], "1.5")
    assert.ok(parts.length >= 1)
})

test("a lessons that ignores SIGTERM is still killed at the timeout", async () => {
    const { script } = fakeLessons("LATE", { sleepSeconds: TIMEOUT_TEST_SLEEP_SECONDS, ignoresTerm: true })
    const startedAt = Date.now()
    const parts = await runHook(script, [{ type: "text", text: "anything" }], SHORT_TIMEOUT_MS)
    assert.equal(parts.length, 1)
    assert.ok(Date.now() - startedAt < TIMEOUT_TEST_SLEEP_SECONDS * MS_PER_SECOND, "returned before the sleep ended")
})

test("malformed parts (null, non-text, non-string text) never throw and are not prompt text", async () => {
    const { script, stdinRecord } = fakeLessons("L")
    await runHook(script, [null, undefined, { type: "text", text: 5 }, { type: "text", text: Symbol("x") }, { type: "text", text: "real" }])
    assert.equal(JSON.parse(readFileSync(stdinRecord, "utf8")).prompt, "real")
})

test("output.parts that is not an array is left alone and does not throw", async () => {
    const { script } = fakeLessons("L")
    const parts = await runHook(script, { not: "an array" })
    assert.deepEqual(parts, { not: "an array" })
})

test("no message id anywhere means no part is pushed; the id from input is used when output has none", async () => {
    const { script } = fakeLessons("L")
    process.env.LESSONS_BIN = script
    const { LessonsHook } = await import(`${PLUGIN_URL}?bin=${encodeURIComponent(script)}`)
    const hooks = await LessonsHook({ directory: "/work/repo" })
    const withoutId = { message: undefined, parts: [{ type: "text", text: "anything" }] }
    await hooks["chat.message"]({ sessionID: SESSION_ID }, withoutId)
    const withInputId = { message: undefined, parts: [{ type: "text", text: "anything" }] }
    await hooks["chat.message"]({ sessionID: SESSION_ID, messageID: "msg_from_input" }, withInputId)
    assert.deepEqual([withoutId.parts.length, withInputId.parts[1]?.messageID], [1, "msg_from_input"])
})

test("a host that passes no output, or a frozen parts array, does not make the hook throw", async () => {
    const { script } = fakeLessons("L")
    process.env.LESSONS_BIN = script
    const { LessonsHook } = await import(`${PLUGIN_URL}?bin=${encodeURIComponent(script)}`)
    const hooks = await LessonsHook({ directory: "/work/repo" })
    await hooks["chat.message"]({ sessionID: SESSION_ID }, undefined)
    await hooks["chat.message"](undefined, { message: { id: MESSAGE_ID }, parts: Object.freeze([{ type: "text", text: "anything" }]) })
})

// opencode 2 path: default export { id, setup(context) } registering the "prompt" session hook.
const setupV2 = async (lessonsBinary) => {
    process.env.LESSONS_BIN = lessonsBinary
    const { default: pluginDefinition } = await import(`${PLUGIN_URL}?v2=${encodeURIComponent(lessonsBinary)}`)
    const registeredHooks = new Map()
    const disposed = []
    const context = {
        location: { directory: "/work/repo" },
        session: {
            hook: async (name, callback) => {
                registeredHooks.set(name, callback)
                return { dispose: async () => disposed.push(name) }
            },
        },
    }
    const cleanup = await pluginDefinition.setup(context)
    return { pluginDefinition, registeredHooks, disposed, cleanup }
}

test("v2: default export has an id and a setup function, and registers only the prompt hook", async () => {
    const { script } = fakeLessons("L")
    const { pluginDefinition, registeredHooks } = await setupV2(script)
    assert.equal(typeof pluginDefinition.id, "string")
    assert.deepEqual([...registeredHooks.keys()], ["prompt"])
})

test("v2: the prompt hook appends the lessons to the prompt text and sends prompt and cwd", async () => {
    const { script, stdinRecord } = fakeLessons("LESSON: use another delimiter")
    const { registeredHooks } = await setupV2(script)
    const event = { prompt: { text: "perl pipe broke" } }
    await registeredHooks.get("prompt")(event)
    assert.equal(event.prompt.text, "perl pipe broke\n\nLESSON: use another delimiter")
    assert.deepEqual(JSON.parse(readFileSync(stdinRecord, "utf8")), { prompt: "perl pipe broke", cwd: "/work/repo" })
})

test("v2: nothing printed, a failing lessons or a malformed event leaves the prompt alone and does not throw", async () => {
    const silent = await setupV2(fakeLessons("").script)
    const failing = await setupV2(fakeLessons("partial", { exitCode: 1 }).script)
    const silentEvent = { prompt: { text: "hello" } }
    const failingEvent = { prompt: { text: "hello" } }
    await silent.registeredHooks.get("prompt")(silentEvent)
    await failing.registeredHooks.get("prompt")(failingEvent)
    for (const malformed of [undefined, {}, { prompt: {} }, { prompt: { text: 5 } }, { prompt: { text: "" } }]) {
        await silent.registeredHooks.get("prompt")(malformed)
    }
    assert.deepEqual([silentEvent.prompt.text, failingEvent.prompt.text], ["hello", "hello"])
})

test("v2: the cleanup returned by setup disposes the hook registration", async () => {
    const { script } = fakeLessons("L")
    const { cleanup, disposed } = await setupV2(script)
    await cleanup()
    assert.deepEqual(disposed, ["prompt"])
})

test("v2: a prompt whose text is not a string is never rewritten, even when lessons would print", async () => {
    const { registeredHooks } = await setupV2(fakeLessons("L").script)
    const numberEvent = { prompt: { text: 5 } }
    await registeredHooks.get("prompt")(numberEvent)
    assert.equal(numberEvent.prompt.text, 5)
})

test("v2: a prompt that cannot be edited (frozen) does not make the hook throw", async () => {
    const { registeredHooks } = await setupV2(fakeLessons("L").script)
    const frozenEvent = { prompt: Object.freeze({ text: "hello" }) }
    await registeredHooks.get("prompt")(frozenEvent)
    assert.equal(frozenEvent.prompt.text, "hello")
})
