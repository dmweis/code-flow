# List the available recipes
[private]
default:
    @just --list

# Install code-flow and cl from this checkout
install:
    cargo install --path . --locked

# Install worktrunk and Herdr, used by the w and W keys
[macos]
install-tools:
    brew install worktrunk herdr
    wt config shell install

# Install worktrunk and Herdr, used by the w and W keys
[linux]
install-tools:
    cargo install worktrunk --locked
    curl -fsSL https://herdr.dev/install.sh | sh
    wt config shell install

# Install the Herdr skill and worktrunk plugin for coding agents
install-skills:
    npx skills add herdrdev/herdr --skill herdr -g
    wt config plugins claude install

# Install code-flow, worktrunk, Herdr and their agent skills
install-all: install install-tools install-skills

# Run the checks CI runs
check:
    cargo fmt --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked
