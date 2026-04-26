/**
 * Integration tests for `src/daemon/worktree-manager.ts`.
 *
 * Each test stands up a `Deno.makeTempDir`-backed bare-source repository
 * with a couple of real commits, then exercises the manager against a
 * second `Deno.makeTempDir`-backed workspace. The tests run actual `git`
 * subprocesses — they require `git` on `PATH` — so the assertions reflect
 * how the bare clone, the worktrees, and `git worktree prune` *really*
 * behave on disk, not against a mock.
 *
 * Coverage targets called out in the W2 brief:
 *
 *  - idempotent `ensureBareClone` (a second call observes the existing
 *    directory and is a no-op);
 *  - concurrent `createWorktreeForIssue` for distinct issues against the
 *    same repo (no shared-state corruption from `git worktree add`);
 *  - `removeWorktree` (idempotent, reclaims directory and metadata);
 *  - `pruneAll` on startup reclaims metadata after an interrupted run.
 */

import { assert, assertEquals, assertNotEquals, assertRejects } from "@std/assert";
import { join } from "@std/path";

import { makeIssueNumber, makeRepoFullName, makeTaskId } from "../../src/types.ts";
import {
  createWorktreeManager,
  GitCommandError,
  type WorktreeManagerImpl,
} from "../../src/daemon/worktree-manager.ts";

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

interface TestRig {
  readonly workspace: string;
  readonly remoteUrl: string;
  readonly remoteDir: string;
  readonly manager: WorktreeManagerImpl;
  cleanup(): Promise<void>;
}

/** Run `git` with the given args and cwd; throw with stderr on failure. */
async function git(args: readonly string[], cwd: string): Promise<string> {
  const command = new Deno.Command("git", {
    args: [...args],
    cwd,
    stdout: "piped",
    stderr: "piped",
    env: {
      // Tests run inside CI environments that may not have a global git
      // identity. Setting these as env vars keeps the test self-contained.
      GIT_AUTHOR_NAME: "makina-test",
      GIT_AUTHOR_EMAIL: "makina-test@example.com",
      GIT_COMMITTER_NAME: "makina-test",
      GIT_COMMITTER_EMAIL: "makina-test@example.com",
    },
  });
  const result = await command.output();
  if (!result.success) {
    const stderr = new TextDecoder().decode(result.stderr);
    throw new Error(
      `git ${args.join(" ")} failed (exit ${result.code}): ${stderr}`,
    );
  }
  return new TextDecoder().decode(result.stdout);
}

/**
 * Build a self-contained source repo with two commits on `main` plus a
 * second branch, then return a `file://` URL the manager can clone from.
 */
async function makeSourceRepo(): Promise<{ dir: string; url: string }> {
  const dir = await Deno.makeTempDir({ prefix: "makina-src-" });
  await git(["init", "--quiet", "--initial-branch=main", dir], dir);
  // First commit.
  await Deno.writeTextFile(join(dir, "README.md"), "# fixture\n");
  await git(["add", "."], dir);
  await git(["commit", "--quiet", "-m", "feat: initial commit"], dir);
  // Second commit on main.
  await Deno.writeTextFile(join(dir, "VERSION"), "0.0.1\n");
  await git(["add", "."], dir);
  await git(["commit", "--quiet", "-m", "chore: bump version"], dir);
  return { dir, url: `file://${dir}` };
}

/** Build a temporary workspace and a manager bound to it. */
async function makeRig(): Promise<TestRig> {
  const workspace = await Deno.makeTempDir({ prefix: "makina-ws-" });
  const source = await makeSourceRepo();
  const manager = createWorktreeManager({ workspace });
  return {
    workspace,
    remoteUrl: source.url,
    remoteDir: source.dir,
    manager,
    async cleanup() {
      await Deno.remove(workspace, { recursive: true });
      await Deno.remove(source.dir, { recursive: true });
    },
  };
}

async function pathExists(path: string): Promise<boolean> {
  try {
    await Deno.lstat(path);
    return true;
  } catch (error) {
    if (error instanceof Deno.errors.NotFound) {
      return false;
    }
    throw error;
  }
}

// ---------------------------------------------------------------------------
// ensureBareClone
// ---------------------------------------------------------------------------

Deno.test("ensureBareClone: clones into <workspace>/repos/<owner>__<name>.git", async () => {
  const rig = await makeRig();
  try {
    const repo = makeRepoFullName("octo/widgets");
    const path = await rig.manager.ensureBareClone(repo, rig.remoteUrl);
    assertEquals(path, join(rig.workspace, "repos", "octo__widgets.git"));
    assert(await pathExists(join(path, "HEAD")), "bare clone has HEAD");
    // `--bare` clones don't create a working tree; sanity-check that.
    assert(!(await pathExists(join(path, ".git"))), "no nested .git in a bare clone");
  } finally {
    await rig.cleanup();
  }
});

Deno.test("ensureBareClone: is idempotent across repeated calls", async () => {
  const rig = await makeRig();
  try {
    const repo = makeRepoFullName("octo/widgets");
    const first = await rig.manager.ensureBareClone(repo, rig.remoteUrl);
    const headPath = join(first, "HEAD");
    const before = (await Deno.lstat(headPath)).mtime;

    // Second and third calls observe the existing clone and short-circuit.
    const second = await rig.manager.ensureBareClone(repo, rig.remoteUrl);
    const third = await rig.manager.ensureBareClone(repo, rig.remoteUrl);
    assertEquals(second, first);
    assertEquals(third, first);

    // mtime on HEAD shouldn't have moved — proof we didn't re-clone.
    const after = (await Deno.lstat(headPath)).mtime;
    assertEquals(after?.getTime(), before?.getTime());
  } finally {
    await rig.cleanup();
  }
});

Deno.test("ensureBareClone: rejects an empty remoteUrl", async () => {
  const rig = await makeRig();
  try {
    await assertRejects(
      () => rig.manager.ensureBareClone(makeRepoFullName("a/b"), ""),
      RangeError,
      "remoteUrl",
    );
  } finally {
    await rig.cleanup();
  }
});

Deno.test("ensureBareClone: refuses to reuse a half-cloned directory", async () => {
  const rig = await makeRig();
  try {
    const repo = makeRepoFullName("octo/widgets");
    // Pre-create the bare clone directory but without a HEAD file —
    // simulating a clone that crashed mid-flight.
    const target = join(rig.workspace, "repos", "octo__widgets.git");
    await Deno.mkdir(target, { recursive: true });

    await assertRejects(
      () => rig.manager.ensureBareClone(repo, rig.remoteUrl),
      Error,
      "no HEAD",
    );
  } finally {
    await rig.cleanup();
  }
});

Deno.test("ensureBareClone: surfaces git failures as GitCommandError", async () => {
  const rig = await makeRig();
  try {
    await assertRejects(
      () =>
        rig.manager.ensureBareClone(
          makeRepoFullName("octo/widgets"),
          "file:///definitely/does/not/exist.git",
        ),
      GitCommandError,
    );
  } finally {
    await rig.cleanup();
  }
});

// ---------------------------------------------------------------------------
// createWorktreeForIssue
// ---------------------------------------------------------------------------

Deno.test("createWorktreeForIssue: lands at the documented path on a fresh branch", async () => {
  const rig = await makeRig();
  try {
    const repo = makeRepoFullName("octo/widgets");
    await rig.manager.ensureBareClone(repo, rig.remoteUrl);
    const path = await rig.manager.createWorktreeForIssue(repo, makeIssueNumber(42));

    assertEquals(
      path,
      join(rig.workspace, "worktrees", "octo__widgets", "issue-42"),
    );
    // Files from the source commit should be checked out in the worktree.
    assert(await pathExists(join(path, "README.md")));
    assert(await pathExists(join(path, "VERSION")));

    // `git rev-parse --abbrev-ref HEAD` inside the worktree confirms the
    // branch name matches the makina/issue-<n> convention.
    const branch = (await git(["rev-parse", "--abbrev-ref", "HEAD"], path)).trim();
    assertEquals(branch, "makina/issue-42");
  } finally {
    await rig.cleanup();
  }
});

Deno.test("createWorktreeForIssue: fails if ensureBareClone has not run", async () => {
  const rig = await makeRig();
  try {
    await assertRejects(
      () => rig.manager.createWorktreeForIssue(makeRepoFullName("a/b"), makeIssueNumber(1)),
      Error,
      "bare clone",
    );
  } finally {
    await rig.cleanup();
  }
});

Deno.test("createWorktreeForIssue: returns the existing path if the worktree is already on disk", async () => {
  const rig = await makeRig();
  try {
    const repo = makeRepoFullName("octo/widgets");
    await rig.manager.ensureBareClone(repo, rig.remoteUrl);
    const path = await rig.manager.createWorktreeForIssue(repo, makeIssueNumber(7));

    // Pretend the agent left a file in the worktree; a second call must
    // **not** clobber it.
    const sentinel = join(path, "in-flight-edit.txt");
    await Deno.writeTextFile(sentinel, "user work in progress");

    const again = await rig.manager.createWorktreeForIssue(repo, makeIssueNumber(7));
    assertEquals(again, path);
    assertEquals(await Deno.readTextFile(sentinel), "user work in progress");
  } finally {
    await rig.cleanup();
  }
});

Deno.test(
  "createWorktreeForIssue: concurrent calls for distinct issues against one repo all succeed",
  async () => {
    const rig = await makeRig();
    try {
      const repo = makeRepoFullName("octo/widgets");
      await rig.manager.ensureBareClone(repo, rig.remoteUrl);

      const issues = [101, 102, 103, 104, 105].map(makeIssueNumber);
      const paths = await Promise.all(
        issues.map((issue) => rig.manager.createWorktreeForIssue(repo, issue)),
      );

      // Every worktree exists, and the paths match the documented layout.
      for (const [i, issue] of issues.entries()) {
        const expected = join(
          rig.workspace,
          "worktrees",
          "octo__widgets",
          `issue-${issue}`,
        );
        assertEquals(paths[i], expected);
        assert(await pathExists(expected), `worktree ${i} on disk`);
      }

      // Every branch was created exactly once. `git worktree list` from
      // the bare clone should report (1 bare + 5 worktrees) = 6 entries.
      const bareDir = join(rig.workspace, "repos", "octo__widgets.git");
      const list = await git(["worktree", "list", "--porcelain"], bareDir);
      const matches = list.match(/^worktree /gm) ?? [];
      assertEquals(matches.length, 6);
    } finally {
      await rig.cleanup();
    }
  },
);

Deno.test(
  "createWorktreeForIssue: concurrent calls for distinct repos run in parallel",
  async () => {
    const wsA = await makeRig();
    const wsB = await makeRig();
    try {
      // Use the same workspace to expose the per-repo locking. Re-bind
      // the second rig's manager to the first rig's workspace so both
      // managers point at the same physical paths but represent
      // different repos.
      const manager = createWorktreeManager({ workspace: wsA.workspace });
      const repoA = makeRepoFullName("octo/alpha");
      const repoB = makeRepoFullName("octo/beta");
      await manager.ensureBareClone(repoA, wsA.remoteUrl);
      await manager.ensureBareClone(repoB, wsB.remoteUrl);

      const [pa, pb] = await Promise.all([
        manager.createWorktreeForIssue(repoA, makeIssueNumber(1)),
        manager.createWorktreeForIssue(repoB, makeIssueNumber(1)),
      ]);
      assertNotEquals(pa, pb);
      assert(await pathExists(pa));
      assert(await pathExists(pb));
    } finally {
      await wsA.cleanup();
      await wsB.cleanup();
    }
  },
);

// ---------------------------------------------------------------------------
// removeWorktree
// ---------------------------------------------------------------------------

Deno.test("removeWorktree: removes the directory and the git metadata entry", async () => {
  const rig = await makeRig();
  try {
    const repo = makeRepoFullName("octo/widgets");
    await rig.manager.ensureBareClone(repo, rig.remoteUrl);
    const path = await rig.manager.createWorktreeForIssue(repo, makeIssueNumber(9));
    const taskId = makeTaskId("task-9");
    rig.manager.registerTaskId(taskId, path);

    assertEquals(rig.manager.worktreePathFor(taskId), path);
    await rig.manager.removeWorktree(taskId);

    assert(!(await pathExists(path)), "worktree directory is gone");
    assertEquals(rig.manager.worktreePathFor(taskId), undefined);

    const bareDir = join(rig.workspace, "repos", "octo__widgets.git");
    const list = await git(["worktree", "list", "--porcelain"], bareDir);
    // Only the bare clone itself remains.
    const matches = list.match(/^worktree /gm) ?? [];
    assertEquals(matches.length, 1);
  } finally {
    await rig.cleanup();
  }
});

Deno.test("removeWorktree: is idempotent for unknown task ids", async () => {
  const rig = await makeRig();
  try {
    await rig.manager.removeWorktree(makeTaskId("never-registered"));
    // No throw, no observable side effect.
  } finally {
    await rig.cleanup();
  }
});

Deno.test("removeWorktree: recovers when the directory was rm'd out from under git", async () => {
  const rig = await makeRig();
  try {
    const repo = makeRepoFullName("octo/widgets");
    await rig.manager.ensureBareClone(repo, rig.remoteUrl);
    const path = await rig.manager.createWorktreeForIssue(repo, makeIssueNumber(5));
    const taskId = makeTaskId("task-5");
    rig.manager.registerTaskId(taskId, path);

    // Simulate an out-of-band rm — common when a user nukes the
    // workspace by hand. The manager should still tear down cleanly.
    await Deno.remove(path, { recursive: true });
    await rig.manager.removeWorktree(taskId);

    const bareDir = join(rig.workspace, "repos", "octo__widgets.git");
    const list = await git(["worktree", "list", "--porcelain"], bareDir);
    const matches = list.match(/^worktree /gm) ?? [];
    assertEquals(matches.length, 1);
    assertEquals(rig.manager.worktreePathFor(taskId), undefined);
  } finally {
    await rig.cleanup();
  }
});

Deno.test("removeWorktree: tolerates registrations outside the workspace root", async () => {
  const rig = await makeRig();
  const stray = await Deno.makeTempDir({ prefix: "makina-stray-" });
  try {
    // Pretend a previous run registered a worktree path that doesn't sit
    // under our `worktrees/` root (e.g. a future relocation feature).
    const taskId = makeTaskId("stray-task");
    rig.manager.registerTaskId(taskId, stray);
    await rig.manager.removeWorktree(taskId);
    assert(!(await pathExists(stray)), "stray path removed");
    assertEquals(rig.manager.worktreePathFor(taskId), undefined);
  } finally {
    // The stray path is already gone; only the workspace cleanup remains.
    await rig.cleanup();
  }
});

// ---------------------------------------------------------------------------
// pruneAll
// ---------------------------------------------------------------------------

Deno.test("pruneAll: reclaims metadata for worktrees deleted between runs", async () => {
  const rig = await makeRig();
  try {
    const repo = makeRepoFullName("octo/widgets");
    await rig.manager.ensureBareClone(repo, rig.remoteUrl);
    const path = await rig.manager.createWorktreeForIssue(repo, makeIssueNumber(3));

    // Simulate an interrupted daemon: the directory is wiped without git
    // ever being told.
    await Deno.remove(path, { recursive: true });

    // Right after deletion the metadata entry is still there (git only
    // notices on the next prune) — confirm that, then run pruneAll.
    const bareDir = join(rig.workspace, "repos", "octo__widgets.git");
    const before = await git(["worktree", "list", "--porcelain"], bareDir);
    assert(before.includes("issue-3"), "metadata still present pre-prune");

    await rig.manager.pruneAll();

    const after = await git(["worktree", "list", "--porcelain"], bareDir);
    assert(!after.includes("issue-3"), "metadata cleared after prune");
  } finally {
    await rig.cleanup();
  }
});

Deno.test("pruneAll: is a no-op when the workspace has no bare clones yet", async () => {
  const workspace = await Deno.makeTempDir({ prefix: "makina-empty-" });
  try {
    const manager = createWorktreeManager({ workspace });
    await manager.pruneAll();
    // Reposroot was not created by pruneAll; it stays absent.
    assert(!(await pathExists(join(workspace, "repos"))));
  } finally {
    await Deno.remove(workspace, { recursive: true });
  }
});

Deno.test("pruneAll: ignores non-bare entries beneath repos/", async () => {
  const rig = await makeRig();
  try {
    // Create a stray non-.git file and a non-.git directory; pruneAll
    // must skip both rather than try to invoke git against them.
    await Deno.mkdir(join(rig.workspace, "repos"), { recursive: true });
    await Deno.writeTextFile(join(rig.workspace, "repos", "stray.txt"), "hi");
    await Deno.mkdir(join(rig.workspace, "repos", "not-a-clone"));

    await rig.manager.pruneAll();
  } finally {
    await rig.cleanup();
  }
});

// ---------------------------------------------------------------------------
// registerTaskId / worktreePathFor
// ---------------------------------------------------------------------------

Deno.test("registerTaskId: rejects an empty path", () => {
  const manager = createWorktreeManager({ workspace: "/tmp" });
  let threw = false;
  try {
    manager.registerTaskId(makeTaskId("t"), "");
  } catch (error) {
    threw = error instanceof RangeError;
  }
  assert(threw, "expected RangeError");
});

Deno.test("registerTaskId: re-registering the same id overwrites the binding", () => {
  const manager = createWorktreeManager({ workspace: "/tmp" });
  const taskId = makeTaskId("t");
  manager.registerTaskId(taskId, "/path/one");
  manager.registerTaskId(taskId, "/path/two");
  assertEquals(manager.worktreePathFor(taskId), "/path/two");
});

Deno.test("createWorktreeManager: rejects an empty workspace path", () => {
  let threw = false;
  try {
    createWorktreeManager({ workspace: "" });
  } catch (error) {
    threw = error instanceof RangeError;
  }
  assert(threw, "expected RangeError");
});

// ---------------------------------------------------------------------------
// gitBinary override
// ---------------------------------------------------------------------------

Deno.test("createWorktreeManager: surfaces a missing gitBinary as a clear error", async () => {
  const workspace = await Deno.makeTempDir({ prefix: "makina-bad-git-" });
  try {
    const manager = createWorktreeManager({
      workspace,
      gitBinary: "/definitely/not/a/real/binary",
    });
    await assertRejects(
      () => manager.ensureBareClone(makeRepoFullName("a/b"), "file:///nope"),
      Error,
    );
  } finally {
    await Deno.remove(workspace, { recursive: true });
  }
});
