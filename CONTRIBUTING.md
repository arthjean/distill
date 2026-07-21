# Contributing to Distill

Thank you for your interest in contributing to Distill! This document provides guidelines for contributing to the project.

## Getting Started

### Prerequisites

- Node.js 20+
- Bun 1.3+

### Development Setup

```bash
# Clone the repository
git clone https://github.com/arthjean/distill.git
cd distill

# Install dependencies
bun install

# Start development servers
bun run dev
```

### Project Structure

```
distill/
├── packages/mcp-server/        # MCP server (npm: distill-mcp)
├── packages/eslint-config/     # Shared ESLint v9 flat configs
└── packages/typescript-config/ # Shared TS presets
```

> The landing page & docs site lives in a separate private repo
> (`arthjean/distill-web`), deployed to distill-mcp.com.

## Development Workflow

### Running Tests

```bash
# Run all tests
bun run test

# Run tests in watch mode (mcp-server)
cd packages/mcp-server && bun run test:watch

# Run with coverage
cd packages/mcp-server && bun run test:coverage
```

### Code Quality

```bash
# Type checking
bun run check-types

# Linting
bun run lint

# Formatting
bun run format
```

### Building

```bash
# Build all packages
bun run build
```

## Branch Strategy

We use a three-branch workflow:

```
main          ← Production releases (protected)
  ↑
dev           ← Integration branch (protected)
  ↑
feature/*     ← Your contributions
fix/*
docs/*
```

| Branch | Purpose | Who can push |
|--------|---------|--------------|
| `main` | Stable releases | Maintainers only |
| `dev` | Integration & testing | Maintainers only |
| `feature/*`, `fix/*`, `docs/*` | Contributions | Everyone (via PR) |

## Making Changes

### For External Contributors

1. **Fork the repository** on GitHub
2. **Clone your fork** locally
3. **Create a feature branch** from `dev`:
   ```bash
   git checkout dev
   git pull origin dev
   git checkout -b feature/your-feature-name
   ```
4. **Make your changes** with clear, focused commits
5. **Add tests** for new functionality
6. **Ensure all tests pass** (`bun run test`)
7. **Run type checks** (`bun run check-types`)
8. **Push to your fork** and submit a PR **targeting `dev`**

### Branch Naming Convention

- `feature/` - New features (e.g., `feature/java-parser`)
- `fix/` - Bug fixes (e.g., `fix/cache-invalidation`)
- `docs/` - Documentation (e.g., `docs/api-reference`)
- `refactor/` - Code refactoring
- `test/` - Test improvements

## Pull Request Guidelines

- **Target branch**: Always target `dev`, not `main`
- Keep PRs focused on a single feature or fix
- Update documentation if needed
- Add tests for new features
- Follow existing code style
- Use [Conventional Commits](https://www.conventionalcommits.org/) format:
  - `feat:` new feature
  - `fix:` bug fix
  - `docs:` documentation
  - `refactor:` code refactoring
  - `test:` test improvements
  - `chore:` maintenance

### PR Flow

```
Your fork → PR to dev → Review → Merge to dev → Release to main
```

## Priority Areas

We especially welcome contributions in:

- **New language parsers** (Java, C#, Kotlin, Ruby)
- **SDK extensions** (new ctx.* functions)
- **Documentation improvements**
- **Bug fixes**

## Reporting Issues

- Use GitHub Issues for bug reports and feature requests
- Include reproduction steps for bugs
- Check existing issues before creating new ones

## Code of Conduct

Be respectful and constructive in all interactions.

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
