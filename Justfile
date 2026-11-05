mod frontend 'frontend/Justfile'
mod backend 'backend/Justfile'
mod proto 'proto/Justfile'
mod dev 'dev/Justfile'
mod apps 'apps/Justfile'
mod engines 'engines/Justfile'
mod installer 'installer/Justfile'
mod ci 'ci/Justfile'

# List all recipes with descriptions.
help:
    @just --list

    @printf '%s\n' '' '--- frontend ---'
    just frontend::help

    @printf '%s\n' '' '--- backend ---'
    just backend::help

    @printf '%s\n' '' '--- proto ---'
    just proto::help

    @printf '%s\n' '' '--- dev ---'
    just dev::help

    @printf '%s\n' '' '--- apps ---'
    just apps::help

    @printf '%s\n' '' '--- engines ---'
    just engines::help

    @printf '%s\n' '' '--- installer ---'
    just installer::help

    @printf '%s\n' '' '--- ci ---'
    just ci::help

# Initialize local environment
bootstrap:
    just backend::bootstrap
    just frontend::bootstrap

# Build repository components.
build:
    just proto::generate
    just frontend::build
    just backend::build

# Run all test suites.
test:
    just proto::generate
    just frontend::test
    just backend::test
    just dev::test

# Run lint across all areas.
lint:
    just proto::generate
    just proto::lint
    just frontend::lint
    just backend::clippy
    just installer::lint

# Check formatting across all areas.
format:
    just proto::generate
    just proto::format
    just frontend::format
    just backend::format

# Repository-wide validation across all areas.
check:
    just proto::generate
    just proto::check
    just frontend::check
    just backend::check
    just dev::check
    just installer::check

# Remove local development state and build outputs.
clean:
    just dev::clean
    just frontend::clean
    just backend::clean
