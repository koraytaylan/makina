/**
 * check_coverage.ts — fail the build if test coverage drops below the threshold.
 *
 * Reads an LCOV report and computes aggregate line and branch coverage,
 * skipping any files listed in `scripts/coverage_exclude.txt`. Exits 0 if
 * both line% and branch% are at or above the threshold; otherwise prints
 * the deficit and exits 1.
 *
 * Usage:
 *   deno run -A scripts/check_coverage.ts <lcov-path> <threshold-percentage>
 *
 * Notes for early waves: when the LCOV has no measurable code after
 * applying the exclude list (Wave 0 sits at exactly that point), the gate
 * reports coverage as "n/a" and passes. As soon as Wave 1 lands real
 * production code the gate becomes meaningful.
 */

import { parseArgs } from "@std/cli/parse-args";

interface FileCoverage {
  readonly path: string;
  readonly linesFound: number;
  readonly linesHit: number;
  readonly branchesFound: number;
  readonly branchesHit: number;
}

interface MutableFileCoverage {
  path: string;
  linesFound: number;
  linesHit: number;
  branchesFound: number;
  branchesHit: number;
}

interface CoverageTotals {
  readonly linesFound: number;
  readonly linesHit: number;
  readonly branchesFound: number;
  readonly branchesHit: number;
}

function parseLcov(content: string): FileCoverage[] {
  const records: FileCoverage[] = [];
  let current: MutableFileCoverage | null = null;

  for (const line of content.split("\n")) {
    if (line.startsWith("SF:")) {
      current = {
        path: line.slice(3),
        linesFound: 0,
        linesHit: 0,
        branchesFound: 0,
        branchesHit: 0,
      };
    } else if (current === null) {
      continue;
    } else if (line.startsWith("LF:")) {
      current.linesFound = Number.parseInt(line.slice(3), 10);
    } else if (line.startsWith("LH:")) {
      current.linesHit = Number.parseInt(line.slice(3), 10);
    } else if (line.startsWith("BRF:")) {
      current.branchesFound = Number.parseInt(line.slice(4), 10);
    } else if (line.startsWith("BRH:")) {
      current.branchesHit = Number.parseInt(line.slice(4), 10);
    } else if (line === "end_of_record") {
      records.push(current);
      current = null;
    }
  }

  return records;
}

async function loadExcludeList(excludePath: string): Promise<Set<string>> {
  try {
    const text = await Deno.readTextFile(excludePath);
    return new Set(
      text
        .split("\n")
        .map((entry) => entry.trim())
        .filter((entry) => entry.length > 0 && !entry.startsWith("#")),
    );
  } catch (error) {
    if (error instanceof Deno.errors.NotFound) {
      return new Set();
    }
    throw error;
  }
}

function isExcluded(filePath: string, excludes: Set<string>): boolean {
  for (const pattern of excludes) {
    if (filePath === pattern || filePath.endsWith("/" + pattern)) {
      return true;
    }
  }
  return false;
}

function aggregate(records: readonly FileCoverage[]): CoverageTotals {
  return records.reduce<CoverageTotals>(
    (acc, record) => ({
      linesFound: acc.linesFound + record.linesFound,
      linesHit: acc.linesHit + record.linesHit,
      branchesFound: acc.branchesFound + record.branchesFound,
      branchesHit: acc.branchesHit + record.branchesHit,
    }),
    { linesFound: 0, linesHit: 0, branchesFound: 0, branchesHit: 0 },
  );
}

function formatRatio(found: number, hit: number): string {
  if (found === 0) {
    return "n/a";
  }
  return `${((hit / found) * 100).toFixed(2)}% (${hit}/${found})`;
}

const EXIT_OK = 0;
const EXIT_BELOW_THRESHOLD = 1;
const EXIT_BAD_USAGE = 2;

const args = parseArgs(Deno.args, {
  string: ["exclude-file"],
  default: { "exclude-file": "scripts/coverage_exclude.txt" },
});
const positional = args._.map(String);
if (positional.length !== 2) {
  console.error("Usage: check_coverage.ts <lcov-path> <threshold-percentage>");
  Deno.exit(EXIT_BAD_USAGE);
}
const lcovPath = positional[0];
const thresholdRaw = positional[1];
if (lcovPath === undefined || thresholdRaw === undefined) {
  // Unreachable given the length check above; satisfies noUncheckedIndexedAccess.
  console.error("Internal error parsing arguments.");
  Deno.exit(EXIT_BAD_USAGE);
}
const threshold = Number.parseFloat(thresholdRaw);
if (Number.isNaN(threshold) || threshold < 0 || threshold > 100) {
  console.error(`Invalid threshold: ${thresholdRaw}`);
  Deno.exit(EXIT_BAD_USAGE);
}

const lcovContent = await Deno.readTextFile(lcovPath);
const excludeFile = args["exclude-file"];
const excludes = await loadExcludeList(excludeFile);

const allRecords = parseLcov(lcovContent);
const eligible = allRecords.filter((record) => !isExcluded(record.path, excludes));
const totals = aggregate(eligible);

const linePercent = totals.linesFound === 0 ? 100 : (totals.linesHit / totals.linesFound) * 100;
const branchPercent = totals.branchesFound === 0
  ? 100
  : (totals.branchesHit / totals.branchesFound) * 100;

console.log(`Files measured: ${eligible.length} (excluded ${allRecords.length - eligible.length})`);
console.log(`Lines:    ${formatRatio(totals.linesFound, totals.linesHit)}`);
console.log(`Branches: ${formatRatio(totals.branchesFound, totals.branchesHit)}`);
console.log(`Threshold: ${threshold}%`);

let failed = false;
if (totals.linesFound > 0 && linePercent < threshold) {
  console.error(`Line coverage ${linePercent.toFixed(2)}% < ${threshold}% threshold.`);
  failed = true;
}
if (totals.branchesFound > 0 && branchPercent < threshold) {
  console.error(`Branch coverage ${branchPercent.toFixed(2)}% < ${threshold}% threshold.`);
  failed = true;
}

if (failed) {
  Deno.exit(EXIT_BELOW_THRESHOLD);
}
console.log("Coverage gate passed.");
Deno.exit(EXIT_OK);
