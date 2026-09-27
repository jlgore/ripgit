import { pipeline } from "@ripgit/ci";

// ripgit's own CI: unit tests, the wasm build, and the end-to-end suite
// (which boots the built worker in Miniflare and drives it with real git).
export default pipeline({
  name: "test",
  on: { push: { branches: ["*"] } },
  jobs: {
    ripgit: {
      timeout: "45 minutes",
      env: {
        // .cargo/config.toml names macOS Homebrew paths for the wasm C
        // toolchain; point cc-rs at the runner image's clang instead.
        CC_wasm32_unknown_unknown: "clang",
        AR_wasm32_unknown_unknown: "llvm-ar",
      },
      steps: [
        { name: "unit tests", run: "cargo test --lib" },
        { name: "install test deps", run: "npm ci" },
        { name: "build worker", run: "npm run build:worker" },
        {
          name: "end-to-end",
          run: "git config --global user.email ci@ripgit.local && git config --global user.name ripgit-ci && NODE_OPTIONS=--experimental-vm-modules npx vitest run",
        },
      ],
    },
    "ci-worker": {
      timeout: "10 minutes",
      steps: [
        { name: "install", run: "cd ci && npm ci" },
        { name: "typecheck", run: "cd ci && npx tsc --noEmit" },
        { name: "unit tests", run: "cd ci && npx vitest run" },
      ],
    },
  },
});
