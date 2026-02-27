# Contributing to Schwab Rust SDK

Thank you for your interest in contributing to the Schwab Rust SDK! This document provides guidelines and instructions for contributing to the project.

## Code of Conduct

Please be respectful and constructive in all interactions. We aim to maintain a welcoming and inclusive environment for all contributors.

## How to Contribute

### Reporting Issues

1. Check existing issues to avoid duplicates
2. Use the issue template when available
3. Provide clear descriptions and steps to reproduce
4. Include relevant error messages and logs
5. Specify your environment (OS, Rust version, etc.)

### Submitting Pull Requests

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/amazing-feature`)
3. Make your changes
4. Add tests for new functionality
5. Ensure all tests pass (`cargo test --all-features`)
6. Run clippy (`cargo clippy -- -D warnings`)
7. Format your code (`cargo fmt`)
8. Commit with clear messages
9. Push to your fork
10. Open a Pull Request

### Development Setup

```bash
# Clone the repository
git clone <repository-url>
cd rustyschwab

# Install Rust (if needed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build the project
cargo build --all-features

# Run tests
cargo test --all-features

# Run examples (requires credentials)
SCHWAB_APP_KEY=your_key SCHWAB_APP_SECRET=your_secret cargo run --example comprehensive
```

## Code Style

### Rust Guidelines

- Follow standard Rust naming conventions
- Use `rustfmt` for formatting
- Use `clippy` for linting
- Prefer explicit error handling over `unwrap()`
- Document public APIs with doc comments
- Add unit tests for new functions
- Keep functions focused and small

### Documentation

- Add doc comments for all public items
- Include examples in doc comments where helpful
- Update README.md for significant changes
- Update CHANGELOG.md following Keep a Changelog format

### Testing

- Write unit tests for new functionality
- Add integration tests for API endpoints
- Test error cases, not just happy paths
- Mock external dependencies in tests
- Ensure tests are deterministic

## Project Structure

```
rustyschwab/
├── crates/
│   ├── schwab-rs/          # Main SDK library
│   │   ├── src/
│   │   │   ├── auth.rs     # OAuth implementation
│   │   │   ├── client.rs   # API client
│   │   │   ├── streaming/  # WebSocket streaming
│   │   │   ├── error.rs    # Error types
│   │   │   └── lib.rs      # Library root
│   │   └── Cargo.toml
│   └── schwab-types/       # Shared type definitions
│       ├── src/
│       │   ├── market_data.rs
│       │   ├── streaming.rs
│       │   ├── accounts.rs
│       │   └── trading.rs
│       └── Cargo.toml
├── examples/               # Usage examples
├── tests/                  # Integration tests
└── Cargo.toml             # Workspace configuration
```

## Commit Messages

Follow conventional commit format:

```
type(scope): description

[optional body]

[optional footer]
```

Types:
- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation changes
- `style`: Code style changes
- `refactor`: Code refactoring
- `test`: Test additions or changes
- `chore`: Maintenance tasks

Example:
```
feat(auth): add token expiry notifications

Implement callback system for token lifecycle events including
access token expiry warnings and refresh token expiration.

Closes #123
```

## API Design Principles

1. **Type Safety**: Use strong types over primitive types
2. **Error Handling**: Return `Result` types with descriptive errors
3. **Async First**: All I/O operations should be async
4. **Builder Pattern**: Use builders for complex configurations
5. **Zero-Cost Abstractions**: Avoid runtime overhead where possible

## Security

- Never commit credentials or tokens
- Validate all user inputs
- Use HTTPS for all API calls
- Follow OWASP security guidelines
- Report security issues privately

## Performance

- Profile before optimizing
- Minimize allocations in hot paths
- Use `Arc` for shared immutable data
- Consider using `parking_lot` for synchronization
- Batch operations where possible

## Documentation Standards

### Code Comments
```rust
/// Brief description of the function.
///
/// # Arguments
///
/// * `param` - Description of parameter
///
/// # Returns
///
/// Description of return value
///
/// # Errors
///
/// Description of possible errors
///
/// # Examples
///
/// ```
/// use schwab_rs::Client;
/// 
/// let client = Client::new(config)?;
/// ```
pub fn example_function(param: &str) -> Result<String> {
    // Implementation
}
```

## Release Process

1. Update version in Cargo.toml files
2. Update CHANGELOG.md
3. Create git tag
4. Push tag to trigger CI/CD
5. Publish to crates.io

## Getting Help

- Check the documentation
- Look through existing issues
- Ask in discussions
- Review the examples

## Recognition

Contributors will be recognized in:
- CHANGELOG.md for significant contributions
- GitHub contributors page
- Release notes

Thank you for contributing to make this SDK better!