// Installed by Pulse (`pulse install opencode`). Remove it with `pulse uninstall opencode`.
//
// It tells Pulse when you send OpenCode a prompt and when the agent works, so
// the time shows up on your profile. It sends session ids, folder names and
// the model name to Pulse on this computer, never your prompts, code or the
// agent's output, and it can never slow down or break OpenCode.
import { spawn } from "node:child_process";

const PULSE = __PULSE_EXE__;

function send(payload) {
  try {
    const child = spawn(PULSE, ["hook", "opencode"], { stdio: ["pipe", "ignore", "ignore"], windowsHide: true });
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify(payload));
  } catch {}
}

// forget the oldest entries instead of growing forever in a long session
function remember(map, key, value) {
  if (map.size >= 1000) map.delete(map.keys().next().value);
  map.set(key, value);
}

export const PulsePlugin = async ({ directory }) => {
  const folders = new Map(); // session -> its folder
  const models = new Map(); // session -> the model you last prompted
  const subagents = new Set(); // sessions the agent started itself: its work, not yours
  const working = new Set(); // sessions with a turn in progress

  const emit = (kind, session) => {
    if (typeof session !== "string" || !session) return;
    send({ kind, session, cwd: folders.get(session) ?? directory, model: models.get(session) });
  };
  const finish = (session) => {
    const turn = working.delete(session);
    if (turn && !subagents.has(session)) emit("turn_ended", session);
  };

  return {
    event: async ({ event }) => {
      const props = event.properties ?? {};
      if (event.type === "session.created") {
        const info = props.info ?? {};
        if (info.directory) remember(folders, info.id, info.directory);
        if (info.parentID) subagents.add(info.id);
        else emit("session_started", info.id);
      } else if (event.type === "session.idle") {
        finish(props.sessionID);
      } else if (event.type === "session.status") {
        if (props.status?.type === "idle") finish(props.sessionID);
      } else if (event.type === "session.deleted") {
        const id = props.info?.id;
        if (!subagents.has(id)) emit("session_ended", id);
        for (const map of [folders, models]) map.delete(id);
        subagents.delete(id);
        working.delete(id);
      }
    },
    "chat.message": async (input, output) => {
      const session = input.sessionID;
      const model = input.model?.modelID ?? output?.message?.model?.modelID;
      if (model) remember(models, session, model);
      working.add(session);
      emit(subagents.has(session) ? "agent_activity" : "prompt_submitted", session);
    },
    "tool.execute.after": async (input) => {
      working.add(input.sessionID);
      emit("agent_activity", input.sessionID);
    },
  };
};
