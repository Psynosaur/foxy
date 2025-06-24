# New Filters Documentation

This document describes the new filters added to Foxy API Gateway, providing comprehensive request/response processing capabilities.

## CompressionFilter

Compresses responses and decompresses requests using gzip and brotli algorithms.

### Configuration

```yaml
filters:
  - type: compression
    config:
      enable_gzip: true          # Enable gzip compression
      enable_br: false           # Enable brotli compression
      min_compress_size: 1024    # Minimum size to compress (bytes)
      max_compress_size: 10485760 # Maximum size to compress (bytes)
      compression_level: 6       # Compression level (1-9 for gzip, 1-11 for brotli)
```

### Features

- Automatic content-type detection for compressible content
- Client Accept-Encoding header support
- Request decompression for incoming compressed payloads
- Configurable size thresholds and compression levels

## RetryFilter

Implements retry logic with exponential backoff for failed requests.

### Configuration

```yaml
filters:
  - type: retry
    config:
      retries: 3                 # Maximum number of retries
      backoff_ms: 100           # Initial backoff delay (ms)
      max_backoff_ms: 30000     # Maximum backoff delay (ms)
      backoff_multiplier: 2.0   # Exponential backoff multiplier
      retry_on_5xx: true        # Retry on 5xx status codes
      retry_on_network_error: true # Retry on network errors
```

### Features

- Exponential backoff with configurable multiplier
- Selective retry based on error types
- Maximum retry limits and backoff caps
- Context preservation for retry attempts

## CircuitBreakerFilter

Implements circuit breaker pattern to prevent cascading failures.

### Configuration

```yaml
filters:
  - type: circuit_breaker
    config:
      failure_threshold: 5      # Failures before opening circuit
      reset_timeout_ms: 60000   # Time before attempting reset (ms)
      success_threshold: 3      # Successes needed to close circuit
      fail_on_5xx: true        # Consider 5xx as failures
      fail_on_network_error: true # Consider network errors as failures
```

### Features

- Per-target circuit tracking
- Three states: Closed, Open, Half-Open
- Automatic recovery attempts
- Configurable failure and success thresholds

## EnhancedRateLimitFilter

Advanced rate limiting with per-client support and 429 responses.

### Configuration

```yaml
filters:
  - type: enhanced_rate_limit
    config:
      req_per_sec: 10.0         # Requests per second limit
      burst_size: 10            # Burst capacity
      strategy: per_ip          # Rate limiting strategy (global, per_ip, per_header)
      client_key_header: "X-Client-ID" # Header for per_header strategy
      include_headers: true     # Include rate limit headers in response
```

### Strategies

- `global`: Single rate limit across all clients
- `per_ip`: Rate limit per client IP address
- `per_header`: Rate limit per custom header value

### Features

- Token bucket algorithm
- Rate limit headers (X-RateLimit-Limit, X-RateLimit-Remaining, X-RateLimit-Reset)
- 429 Too Many Requests responses
- Configurable client identification

## CachingFilter

TTL-based response caching with configurable cache keys.

### Configuration

```yaml
filters:
  - type: caching
    config:
      ttl_secs: 300             # Time-to-live in seconds
      cache_key: "{method}:{path}:{query}" # Cache key pattern
      max_cache_size: 1000      # Maximum number of cached entries
      max_response_size: 1048576 # Maximum response size to cache (bytes)
      cache_get_only: true      # Only cache GET requests
      cacheable_status_codes: [200, 203, 300, 301, 302, 404, 410]
```

### Features

- In-memory caching with TTL expiration
- Configurable cache key patterns with placeholders
- Size-based cache limits
- Status code filtering
- Automatic cache cleanup

## AuthFilter

JWT token validation with JWKS support.

### Configuration

```yaml
filters:
  - type: auth
    config:
      jwt_issuer: "https://auth.example.com"
      jwt_audience: "api"
      jwks_url: "https://auth.example.com/.well-known/jwks.json"
      jwt_secret: "shared-secret" # Alternative to JWKS
      jwt_algorithm: "RS256"    # JWT algorithm
      bypass_paths: ["/health", "/metrics"] # Paths to bypass auth
      auth_header: "authorization" # Header containing JWT
      validate_exp: true        # Validate expiration
      validate_nbf: true        # Validate not-before
      leeway: 60               # Time leeway in seconds
```

### Features

- JWT validation with multiple algorithms
- JWKS endpoint support for key fetching
- Static secret support for HMAC algorithms
- Path-based bypass rules
- Claims injection into request context

## CorsFilter

Cross-Origin Resource Sharing (CORS) header management.

### Configuration

```yaml
filters:
  - type: cors
    config:
      allowed_origins: ["https://example.com", "https://app.example.com"]
      allowed_methods: ["GET", "POST", "PUT", "DELETE", "OPTIONS"]
      allowed_headers: ["Content-Type", "Authorization", "X-Requested-With"]
      exposed_headers: ["X-Total-Count"]
      allow_credentials: false  # Allow credentials
      max_age: 86400           # Preflight cache duration (seconds)
```

### Features

- Origin validation and header injection
- Preflight request handling
- Credential support configuration
- Configurable cache duration for preflight responses

## MetricsFilter

Prometheus metrics collection for requests and responses.

### Configuration

```yaml
filters:
  - type: metrics
    config:
      metrics_endpoint: "/metrics" # Endpoint to serve metrics
      histogram_buckets: [0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]
      include_path_labels: true  # Include path in metrics labels
      include_method_labels: true # Include method in metrics labels
      include_status_labels: true # Include status in metrics labels
```

### Metrics Collected

- `foxy_http_requests_total`: Total number of HTTP requests
- `foxy_http_request_duration_seconds`: Request duration histogram
- `foxy_http_response_size_bytes`: Response size histogram

### Features

- Prometheus-compatible metrics format
- Configurable label inclusion
- Built-in metrics endpoint
- Path normalization to reduce cardinality

## BodyTransformFilter

Request/response body transformation with multiple methods.

### Configuration

```yaml
filters:
  - type: body_transform
    config:
      transformation:
        type: regex_replace
        pattern: "old_value"
        replacement: "new_value"
        global: true
      transform_request: false  # Transform request bodies
      transform_response: true  # Transform response bodies
      content_types: ["text/plain", "application/json"] # Allowed content types
      max_transform_size: 1048576 # Maximum body size to transform
```

### Transformation Types

#### Regex Replace
```yaml
transformation:
  type: regex_replace
  pattern: "\\b(\\w+)@example\\.com\\b"
  replacement: "$1@newdomain.com"
  global: true
```

#### JSON Path
```yaml
transformation:
  type: json_path
  path: "user.email"
  value: "redacted@example.com"
  operation: set  # set, delete, append
```

#### Base64 Encoding/Decoding
```yaml
transformation:
  type: base64
  decode: false  # true for decode, false for encode
```

### Features

- Multiple transformation methods
- Content-type filtering
- Size-based transformation limits
- Separate request/response transformation control

## Usage Examples

### Basic Configuration

```yaml
routes:
  - id: api-route
    target: "http://api.backend.com"
    predicates:
      - type: path
        config:
          pattern: "/api/*"
    filters:
      - type: compression
        config:
          enable_gzip: true
      - type: enhanced_rate_limit
        config:
          req_per_sec: 100
          strategy: per_ip
      - type: cors
        config:
          allowed_origins: ["*"]
      - type: metrics
        config:
          metrics_endpoint: "/metrics"
```

### Advanced Security Setup

```yaml
routes:
  - id: secure-api
    target: "http://secure.backend.com"
    predicates:
      - type: path
        config:
          pattern: "/secure/*"
    filters:
      - type: auth
        config:
          jwt_issuer: "https://auth.company.com"
          jwks_url: "https://auth.company.com/.well-known/jwks.json"
          bypass_paths: ["/secure/health"]
      - type: circuit_breaker
        config:
          failure_threshold: 3
          reset_timeout_ms: 30000
      - type: enhanced_rate_limit
        config:
          req_per_sec: 10
          strategy: per_header
          client_key_header: "X-API-Key"
```

## Filter Ordering

The order of filters in the configuration determines their execution order:

1. **Pre-filters** execute in the order specified
2. **Post-filters** execute in reverse order

Recommended ordering:
1. `metrics` (for timing)
2. `cors` (for preflight handling)
3. `auth` (early security check)
4. `enhanced_rate_limit` (prevent abuse)
5. `circuit_breaker` (prevent cascading failures)
6. `retry` (handle failures)
7. `caching` (serve cached responses)
8. `compression` (compress responses)
9. `body_transform` (final transformations)

## Error Handling

All filters implement proper error handling and return appropriate HTTP status codes:

- `401 Unauthorized`: Authentication failures
- `403 Forbidden`: Authorization failures
- `429 Too Many Requests`: Rate limiting
- `500 Internal Server Error`: Circuit breaker open
- `502 Bad Gateway`: Upstream failures

## Performance Considerations

- **Compression**: CPU overhead for compression/decompression
- **Caching**: Memory usage for cached responses
- **Metrics**: Small overhead for metric collection
- **Auth**: Network latency for JWKS fetching
- **Body Transform**: CPU overhead for transformations

Monitor resource usage and adjust configurations accordingly.
