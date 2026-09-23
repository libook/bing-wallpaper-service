# Upstream failures return 502, not 500

When the wallpaper source fails, the service returns a precise HTTP status code
that reflects *what* failed and *who owns it*, rather than a blanket server
failure.

- `400` — the client sent parameters this service cannot interpret
- `404` — the requested index does not exist
- `500` — the Bing API contract changed under us; this is our bug
- `502` — the upstream (Bing API, or the image host) is unavailable

`502 Bad Gateway` is the standard code for "this service is up and acting as a
gateway, but its upstream failed". It is deliberately not `500`: `500` is
reserved for failures this service owns, so a client can distinguish "the service
is down" from "the service is up and Bing is not". The handler adapter owns this
translation; `SourceError` does not know HTTP.

## Considered options

- **Blanket `500` for every server-side failure.** Rejected: it erases the
  distinction between "we broke" and "Bing broke", which is exactly the
  information a client (and the operator) needs.
- **Never emit `5xx`; always `200` with an error body.** Rejected: the service's
  response body is a wallpaper URL or image bytes, with no error channel. A `200`
  carrying a failure would lie to clients that key off the status code.
- **`503` for upstream failures.** Rejected: `503` means "I am unavailable", which
  is false here — the service itself is healthy. `502` is the code that says "I
  am a gateway and my upstream gave me nothing".

## Consequences

- The handler grows three `match` arms (query, URL, image). Each arm is one
  place to edit when the failure vocabulary changes.
- `SourceError` gains `Network` and `ImageFetch` variants; `HttpError` stays a
  separate type so the module translates rather than leaking the HTTP layer's
  error shape across the seam.