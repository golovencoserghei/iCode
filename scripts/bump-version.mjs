#!/usr/bin/env node
// Usage: node scripts/bump-version.mjs <patch|minor|major|auto> [--dry]
// `auto` picks the level from Conventional Commit subjects since the last v* tag:
//   `type!:` or a "BREAKING CHANGE" line -> major, `feat:` -> minor, anything else -> patch.
// CI runs `auto` on every push to the main branch; a developer can pass a level by hand.
import { readFileSync, writeFileSync, existsSync } from 'node:fs'
import { execFileSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const dry = process.argv.includes('--dry')
let level = process.argv[2]
if (!['patch', 'minor', 'major', 'auto'].includes(level)) {
  console.error('usage: bump-version.mjs <patch|minor|major|auto> [--dry]')
  process.exit(1)
}

const git = (...args) => execFileSync('git', args, { cwd: root, encoding: 'utf8' })

function detectLevel() {
  let tag = ''
  try { tag = git('tag', '--list', 'v*', '--sort=-v:refname').split('\n')[0].trim() } catch {}
  const log = git('log', tag ? `${tag}..HEAD` : 'HEAD', '--format=%s%n%b%x00')
  let found = 'patch'
  for (const commit of log.split('\0')) {
    if (/^chore\(release\)/.test(commit.trim())) continue
    if (/^\w+(\([^)]*\))?!:/m.test(commit) || /^BREAKING[ -]CHANGE/m.test(commit)) return 'major'
    if (/^feat(\([^)]*\))?:/m.test(commit)) found = 'minor'
  }
  return found
}

function bump([maj, min, pat]) {
  return level === 'major' ? `${maj + 1}.0.0` : level === 'minor' ? `${maj}.${min + 1}.0` : `${maj}.${min}.${pat + 1}`
}

const read = (p) => JSON.parse(readFileSync(join(root, p), 'utf8'))
const write = (p, o) => writeFileSync(join(root, p), JSON.stringify(o, null, 2) + '\n')
if (level === 'auto') level = detectLevel()

const tomlPath = join(root, 'Cargo.toml')
const toml = readFileSync(tomlPath, 'utf8')
const m = toml.match(/(\[workspace\.package\][^[]*?\nversion = ")(\d+)\.(\d+)\.(\d+)(")/)
if (!m) {
  console.error('workspace.package version not found in Cargo.toml')
  process.exit(1)
}
const next = bump([m[2], m[3], m[4]].map(Number))
if (dry) {
  console.log(`${level} -> ${next}`)
  process.exit(0)
}
writeFileSync(tomlPath, toml.replace(m[0], `${m[1]}${next}${m[5]}`))

const lockPath = join(root, 'Cargo.lock')
const lock = readFileSync(lockPath, 'utf8')
writeFileSync(lockPath, lock.replace(/(name = "icode[a-z-]*"\nversion = ")[^"]+(")/g, `$1${next}$2`))
console.log(next)
