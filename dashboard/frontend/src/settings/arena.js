// Desktop client for `agentpit arena`.
//
// Every call goes through the bundled CLI, so the arena's rules live in exactly one place. The
// identities behind a round's blind labels are NOT returned by `arenaRound` — the CLI withholds
// them — and arrive only from `arenaReveal`, which the UI calls after the voting is finished.
// Nothing here can express "vote for codex": a vote is cast by label.

function invoke() {
  return window.__TAURI__?.core?.invoke;
}

function call(cmd, args) {
  const fn = invoke();
  if (!fn) return Promise.reject(new Error("デスクトップアプリの外では実行できません。"));
  return fn(cmd, args);
}

export const arenaTemplates = () => call("arena_templates");
export const arenaRounds = () => call("arena_rounds");
export const arenaRound = (roundId) => call("arena_round", { roundId });
export const arenaReveal = (roundId) => call("arena_reveal", { roundId });
export const arenaLeaderboard = () => call("arena_leaderboard");

export const arenaVote = (roundId, winner, loser, tie = false) =>
  call("arena_vote", { req: { round_id: roundId, winner, loser, tie } });

export const arenaRun = ({ task, template, target, contenders, cwd }) =>
  call("arena_run", { task, template, target, contenders, cwd });

// Which comparisons in a round still need a verdict. The CLI reports how many votes were cast,
// not which pairs they covered, so votes are consumed in matchup order — the same order the
// terminal walks them in, which keeps the two front-ends in agreement about what is left.
export function pendingMatchups(round) {
  const pairs = round?.matchups || [];
  return pairs.slice(round?.votes || 0);
}
