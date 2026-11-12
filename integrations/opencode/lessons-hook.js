// opencode plugin: before each user message, ask `lessons hook user-prompt-submit` for relevant lessons and
// add them to the message (opencode 1: as a synthetic text part; opencode 2: appended to the prompt text, see the
// default export at the end). Silent when nothing is relevant, when `lessons` is missing,
// slow or failing: a lessons problem must never block or break a prompt.
//
// Install: copy this one file into opencode's plugins directory (scripts/setup.sh --agents opencode does it).
// opencode 1 loads every function export of a file in that directory as a plugin, so this file exports only the plugin function and the default object opencode 2 needs.

import { execFile } from "node:child_process"
import { randomUUID } from "node:crypto"

const LESSONS_COMMAND = process.env.LESSONS_BIN || "lessons"
const DEFAULT_HOOK_TIMEOUT_MS = 10_000
const MAX_TIMER_MS = 2 ** 31 - 1 // a larger delay silently becomes 1 ms in Node
const MAX_OUTPUT_BYTES = 32 * 1024 // the hook prints at most 8,000 characters (hook.rs MAX_OUTPUT_CHARS), 4 bytes each in the worst case; more is a fault and is dropped
const PART_ID_PREFIX = "prt_"

// Only a whole number of milliseconds from 1 up to MAX_TIMER_MS is a timeout; empty, garbage, zero, negative or too large falls back to the default.
const timeoutMsFromEnv = () => {
    const requestedMs = Math.trunc(Number(process.env.LESSONS_HOOK_TIMEOUT_MS)) // execFile throws on a fraction and Infinity
    return requestedMs > 0 && requestedMs <= MAX_TIMER_MS ? requestedMs : DEFAULT_HOOK_TIMEOUT_MS
}

// LESSONS_HOOK_TIMEOUT_MS exists so a test can use a short timeout; it is read per call.
const runLessonsHook = (promptText, workingDirectory) =>
    new Promise((resolve) => {
        const child = execFile(
            LESSONS_COMMAND,
            ["hook", "user-prompt-submit"],
            { timeout: timeoutMsFromEnv(), killSignal: "SIGKILL", maxBuffer: MAX_OUTPUT_BYTES },
            (failure, stdoutText) => resolve(failure ? "" : stdoutText.trim()),
        )
        child.stdin?.on("error", () => {}) // a missing binary closes stdin early; the callback above already reports it
        child.stdin?.end(JSON.stringify({ prompt: promptText, cwd: workingDirectory }))
    })

const promptTextOf = (parts) =>
    (Array.isArray(parts) ? parts : [])
        .filter((part) => part?.type === "text" && typeof part.text === "string" && !part.synthetic && !part.ignored)
        .map((part) => part.text)
        .join("\n")

export const LessonsHook = async ({ directory }) => {
    const addLessonsToMessage = async (input, output) => {
        const messageID = output.message?.id ?? input.messageID
        const promptText = promptTextOf(output.parts)
        const lessonsText = promptText === "" ? "" : await runLessonsHook(promptText, directory)
        // A part with no message to belong to would be malformed, so it is not pushed.
        lessonsText === "" ||
            messageID === undefined ||
            output.parts.push({
                id: `${PART_ID_PREFIX}${randomUUID()}`,
                sessionID: input.sessionID,
                messageID,
                type: "text",
                text: lessonsText,
                synthetic: true,
            })
    }
    return {
        // The one guard that keeps the promise: whatever the host hands over, a lessons problem must never break a prompt.
        "chat.message": async (input, output) => addLessonsToMessage(input, output).catch(() => {}),
    }
}

// opencode 2 loads a plugin from its default export: an id and a `setup(context)` that registers hooks. Its server
// hooks (the `chat.message` above) are not called there; the `prompt` session hook is, and it may edit the prompt
// text before the message is stored, so the lessons are appended to it. The named export above stays for opencode 1;
// the default object has no `server` key, so opencode 1 cannot register the hook twice.
export default {
    id: "lessons.hook",
    setup: async (context) => {
        const addLessonsToPrompt = async (event) => {
            const promptText = event?.prompt?.text
            const lessonsText = typeof promptText === "string" && promptText !== "" ? await runLessonsHook(promptText, context.location?.directory) : ""
            lessonsText === "" || (event.prompt.text = `${promptText}\n\n${lessonsText}`)
        }
        // Same guard as the opencode 1 hook: whatever the host hands over, a lessons problem must never break a prompt.
        const registration = await context.session.hook("prompt", (event) => addLessonsToPrompt(event).catch(() => {}))
        return () => registration.dispose()
    },
}
