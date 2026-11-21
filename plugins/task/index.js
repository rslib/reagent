// A task plugin: hands a self-contained job to a read-only helper session and
// answers what that helper said. See docs/plugin-sessions.md.

import process from "node:process";

// The first words of every helper: what it is, which tools it has, and that
// its last message is the whole answer.
const ROLE =
  "You are a helper. Do the job below with the read, find and grep tools. " +
  "Your last message is the whole answer the caller gets: it does not see your " +
  "tool output, so put every finding it needs there.";

// The tools a helper may use, whatever the run it starts from has.
const TOOLS = "read,find,grep";

// A reason a person reads, from a rejection.
function said(error) {
  if (error && typeof error.message === "string") return error.message;
  return String(error);
}

// The env value of a variable the manifest lets this plugin read, trimmed, or
// an empty string.
function named(name) {
  const value = process.env[name];
  return typeof value === "string" ? value.trim() : "";
}

export default function (reagent) {
  reagent.registerTool({
    name: "task",
    description:
      "Hands a self-contained job to a helper session that can only read files (read, find, grep) and returns its final answer. Use it for a search or a reading job whose details you do not need in your own context. The helper sees none of this conversation, so put everything it needs in prompt.",
    parameters: {
      type: "object",
      properties: {
        prompt: {
          type: "string",
          description:
            "The whole job: what to find or read, and what the answer must contain.",
        },
      },
      required: ["prompt"],
    },
    timeoutMs: 1800000,
    run: async (args, context) => {
      const prompt = typeof args.prompt === "string" ? args.prompt.trim() : "";
      if (prompt === "") {
        return { text: "task needs a prompt", isError: true };
      }
      const spec = { task: `${ROLE}\n\n${prompt}`, tools: TOOLS };
      // The two variables say which provider and model a helper runs on, so a
      // run on a scripted provider can give its helpers another one.
      const provider = named("REAGENT_TASK_PROVIDER");
      const model = named("REAGENT_TASK_MODEL");
      if (provider !== "") spec.provider = provider;
      if (model !== "") spec.model = model;
      let id;
      try {
        id = await reagent.sessions.start(spec);
      } catch (error) {
        return {
          text: `task could not start a helper: ${said(error)}`,
          isError: true,
        };
      }
      // A cancel of this call stops the helper too, so a cancelled run leaves
      // no helper of its own behind.
      context.onCancel(() => reagent.sessions.cancel(id));
      const { reason, text } = await reagent.sessions.result(id);
      if (reason === "end_turn") {
        if (typeof text === "string" && text.trim() !== "") return text;
        return { text: "the helper gave no answer", isError: true };
      }
      const stop = `the helper stopped (${reason})`;
      const failed = reason === "error" || reason === "cancelled";
      if (typeof text === "string" && text !== "") {
        return { text: `${stop}:\n${text}`, isError: failed };
      }
      return { text: stop, isError: failed };
    },
  });
}
