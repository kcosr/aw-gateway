#!/usr/bin/env node
/**
 * Release script for aw-gateway.
 *
 * Usage:
 *   node scripts/release.mjs current  # Release current Cargo.toml version
 *   node scripts/release.mjs patch
 *   node scripts/release.mjs minor
 *   node scripts/release.mjs major
 *   node scripts/release.mjs 0.2.3
 *
 * Steps:
 * 1. Check for a clean main branch and required tools
 * 2. Optionally bump Cargo.toml and Cargo.lock
 * 3. Update CHANGELOG.md: [Unreleased] -> [version] - date
 * 4. Commit and tag
 * 5. Push commit and tag to origin
 * 6. Create GitHub release with notes from CHANGELOG.md
 * 7. Add a new [Unreleased] section
 * 8. Commit and push the next-cycle changelog
 */

import { execSync } from "child_process";
import { existsSync, readFileSync, unlinkSync, writeFileSync } from "fs";
import { dirname, join } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, "..");
const PACKAGE_NAME = "aw-gateway";
const REPO = "kcosr/aw-gateway";
const RELEASE_BRANCH = "main";
const RELEASE_ARG = process.argv[2];
const BUMP_ARGS = new Set(["major", "minor", "patch"]);
const VERSION_ARG = /^\d+\.\d+\.\d+(-[\w.]+)?$/;
const cargoTomlPath = join(ROOT, "Cargo.toml");
const cargoLockPath = join(ROOT, "Cargo.lock");

if (
	!RELEASE_ARG ||
	(!BUMP_ARGS.has(RELEASE_ARG) &&
		RELEASE_ARG !== "current" &&
		!VERSION_ARG.test(RELEASE_ARG))
) {
	console.error("Usage: node scripts/release.mjs <current|major|minor|patch|X.Y.Z>");
	process.exit(1);
}

function run(cmd, options = {}) {
	console.log(`$ ${cmd}`);
	try {
		return execSync(cmd, {
			encoding: "utf-8",
			stdio: options.silent ? "pipe" : "inherit",
			cwd: ROOT,
			...options,
		});
	} catch (error) {
		if (!options.ignoreError) {
			console.error(`Command failed: ${cmd}`);
			process.exit(1);
		}
		return null;
	}
}

function getVersion() {
	const content = readFileSync(cargoTomlPath, "utf-8");
	const match = content.match(/\[package\][\s\S]*?\nversion\s*=\s*"([^"]+)"/);
	if (!match) {
		console.error("Could not find version in Cargo.toml [package] section");
		process.exit(1);
	}
	return match[1];
}

function escapeRegex(value) {
	return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function parseVersion(version) {
	const match = version.match(/^(\d+)\.(\d+)\.(\d+)(.*)$/);
	if (!match) {
		return null;
	}
	return {
		major: Number.parseInt(match[1], 10),
		minor: Number.parseInt(match[2], 10),
		patch: Number.parseInt(match[3], 10),
		suffix: match[4] || "",
	};
}

function formatVersion(parts) {
	return `${parts.major}.${parts.minor}.${parts.patch}${parts.suffix}`;
}

function updateCargoTomlVersion(newVersion) {
	let content = readFileSync(cargoTomlPath, "utf-8");
	const versionRegex = /(\[package\][\s\S]*?\nversion\s*=\s*")[^"]*(")/;
	if (!versionRegex.test(content)) {
		console.error("Cargo.toml [package] version not found");
		process.exit(1);
	}

	content = content.replace(versionRegex, `$1${newVersion}$2`);
	writeFileSync(cargoTomlPath, content, "utf-8");
}

function updateCargoLockVersion(newVersion) {
	if (!existsSync(cargoLockPath)) {
		return;
	}

	let content = readFileSync(cargoLockPath, "utf-8");
	const packageRegex = new RegExp(
		`(\\[\\[package\\]\\]\\nname = "${escapeRegex(PACKAGE_NAME)}"\\nversion = ")[^"]*(")`
	);
	if (!packageRegex.test(content)) {
		console.error(`Cargo.lock package entry not found for ${PACKAGE_NAME}`);
		process.exit(1);
	}

	content = content.replace(packageRegex, `$1${newVersion}$2`);
	writeFileSync(cargoLockPath, content, "utf-8");
}

function bumpVersion(currentVersion, bumpArg) {
	if (VERSION_ARG.test(bumpArg)) {
		return bumpArg;
	}

	const parts = parseVersion(currentVersion);
	if (!parts) {
		console.error(`Current version "${currentVersion}" is not valid semver (X.Y.Z)`);
		process.exit(1);
	}

	switch (bumpArg) {
		case "patch":
			parts.patch += 1;
			parts.suffix = "";
			break;
		case "minor":
			parts.minor += 1;
			parts.patch = 0;
			parts.suffix = "";
			break;
		case "major":
			parts.major += 1;
			parts.minor = 0;
			parts.patch = 0;
			parts.suffix = "";
			break;
		default:
			console.error(`Invalid release argument: ${bumpArg}`);
			process.exit(1);
	}

	return formatVersion(parts);
}

function ensureCleanMain() {
	const branch = run("git branch --show-current", { silent: true }).trim();
	if (branch !== RELEASE_BRANCH) {
		console.error(
			`Error: releases must be run from ${RELEASE_BRANCH}; current branch is ${branch || "(detached)"}.`
		);
		process.exit(1);
	}

	const status = run("git status --porcelain", { silent: true });
	if (status && status.trim()) {
		console.error("Error: Uncommitted changes detected. Commit or stash first.");
		console.error(status);
		process.exit(1);
	}
}

function ensureTools() {
	run("git --version", { silent: true });
	run("node --version", { silent: true });
	run("gh --version", { silent: true });
}

function ensureTagAvailable(version) {
	const tagExists = run(`git rev-parse -q --verify refs/tags/v${version}`, {
		silent: true,
		ignoreError: true,
	});
	if (tagExists) {
		console.error(`Error: tag v${version} already exists.`);
		process.exit(1);
	}
}

function updateChangelogForRelease(version) {
	const changelogPath = join(ROOT, "CHANGELOG.md");
	const date = new Date().toISOString().split("T")[0];
	let content = readFileSync(changelogPath, "utf-8");

	if (!content.includes("## [Unreleased]")) {
		console.error("Error: No [Unreleased] section found in CHANGELOG.md");
		process.exit(1);
	}
	if (content.includes(`## [${version}]`)) {
		console.error(`Error: CHANGELOG.md already contains a [${version}] section`);
		process.exit(1);
	}

	const unreleasedMatch = content.match(/## \[Unreleased\]\n([\s\S]*?)(?=\n## \[|$)/);
	if (!unreleasedMatch || unreleasedMatch[1].trim() === "_No unreleased changes._") {
		console.error("Error: CHANGELOG.md has no release notes under [Unreleased]");
		process.exit(1);
	}

	content = content.replace(/## \[Unreleased\]/, `## [${version}] - ${date}`);
	writeFileSync(changelogPath, content);
	console.log(`  Updated CHANGELOG.md: [Unreleased] -> [${version}] - ${date}`);
}

function extractReleaseNotes(version) {
	const changelogPath = join(ROOT, "CHANGELOG.md");
	const content = readFileSync(changelogPath, "utf-8");
	const versionEscaped = version.replace(/\./g, "\\.");
	const regex = new RegExp(
		`## \\[${versionEscaped}\\][^\\n]*\\n([\\s\\S]*?)(?=\\n## \\[|$)`
	);
	const match = content.match(regex);

	if (!match) {
		console.error(`Error: Could not extract release notes for v${version}`);
		process.exit(1);
	}

	return match[1].trim();
}

function addUnreleasedSection() {
	const changelogPath = join(ROOT, "CHANGELOG.md");
	let content = readFileSync(changelogPath, "utf-8");

	const unreleasedSection = "## [Unreleased]\n\n_No unreleased changes._\n\n";
	content = content.replace(/^(# Changelog\n\n)/, `$1${unreleasedSection}`);

	writeFileSync(changelogPath, content);
	console.log("  Added [Unreleased] section to CHANGELOG.md");
}

console.log("\n=== aw-gateway Release Script ===\n");

console.log("Checking release prerequisites...");
ensureCleanMain();
ensureTools();
console.log("  Clean main branch and required tools available\n");

if (RELEASE_ARG === "current") {
	console.log("Using current Cargo.toml version...");
} else {
	const currentVersion = getVersion();
	const nextVersion = bumpVersion(currentVersion, RELEASE_ARG);
	console.log(`Bumping version: ${currentVersion} -> ${nextVersion}`);
	updateCargoTomlVersion(nextVersion);
	updateCargoLockVersion(nextVersion);
}
const version = getVersion();
ensureTagAvailable(version);
console.log(`  Release version: ${version}\n`);

console.log("Updating CHANGELOG.md...");
updateChangelogForRelease(version);
console.log();

console.log("Committing and tagging...");
run("git add Cargo.toml Cargo.lock CHANGELOG.md");
run(`git commit -m "Release v${version}"`);
run(`git tag v${version}`);
console.log();

console.log("Pushing to remote...");
run(`git push origin ${RELEASE_BRANCH}`);
run(`git push origin v${version}`);
console.log();

console.log("Creating GitHub release...");
const releaseNotes = extractReleaseNotes(version);
const notesFile = join(ROOT, ".release-notes-tmp.md");
writeFileSync(notesFile, releaseNotes);
run(
	`gh release create v${version} --repo ${REPO} --title "v${version}" --notes-file "${notesFile}"`
);
unlinkSync(notesFile);
console.log();

console.log("Adding [Unreleased] section for next cycle...");
addUnreleasedSection();
console.log();

console.log("Committing changelog update...");
run("git add CHANGELOG.md");
run('git commit -m "Prepare for next release"');
run(`git push origin ${RELEASE_BRANCH}`);
console.log();

console.log(`=== Released v${version} ===`);
console.log(`https://github.com/${REPO}/releases/tag/v${version}`);
