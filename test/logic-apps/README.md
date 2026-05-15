# Test Logic Apps Fixtures

Sample workflows for local development with `func start`.

## Workflows

| Folder | What it does |
|---|---|
| `hello-world` | HTTP trigger → echo response. No external dependencies. |
| `write-to-storage` | HTTP trigger → upload blob to Azurite. |
| `send-to-bus` | HTTP trigger → send message to Service Bus queue. |
| `write-to-cosmos` | HTTP trigger → create/upsert document in Cosmos DB emulator. |

## Prerequisites

- **Azurite** running (Azure Storage emulator)
- **Cosmos DB emulator** running — use the `vnext-preview` Linux Docker image:
  ```
  docker run -p 8081:8081 -p 1234:1234 mcr.microsoft.com/cosmosdb/linux/azure-cosmos-emulator:vnext-preview
  ```

## Cosmos DB

The `write-to-cosmos` workflow uses a plain **HTTP action** against the Cosmos DB REST API rather
than the `AzureCosmosDB` service provider. The HMAC-SHA256 authorization header is computed
inside the workflow using the built-in `hmacSha256()` expression function.

This is the only approach that works with the Linux Docker emulator locally — see the
"Lessons learned" section below for why the service provider cannot be used.

## Cosmos DB: lessons learned

These took trial and error to get right — do not change without testing:

### 1. Service provider ID is `AzureCosmosDB`, not `CosmosDb`

In both `connections.json` and `workflow.json` the `serviceProviderId` must be exactly:
```
/serviceProviders/AzureCosmosDB
```
The runtime rejects `cosmosDb`, `CosmosDb`, and `documentDb` with a validation error.

### 2. Operation ID is `CreateOrUpdateDocument`, not `createDocument`

The only valid write operation exposed by the installed bundle (`v1.161.x`) is:
```
CreateOrUpdateDocument
```
Other names (`createDocument`, `upsertDocument`) are rejected.

### 3. Connection uses `connectionString` parameter set, not endpoint + key

`connections.json` must use:
```json
{
  "parameterSetName": "connectionString",
  "parameterValues": {
    "connectionString": "@appsetting('COSMOS_CONNECTION_STRING')"
  }
}
```
The `accountEndpoint` + `authenticationPolicy` layout is parsed by the app but not accepted by the func runtime for initialising the connection.

### 4. The `vnext-preview` emulator serves plain HTTP

The Docker emulator (`mcr.microsoft.com/cosmosdb/linux/azure-cosmos-emulator:vnext-preview`) serves
**plain HTTP** on port 8081 — not HTTPS. The connection string must use `http://`:
```
AccountEndpoint=http://localhost:8081/;AccountKey=C2y6y...
```
Using `https://` causes an SSL handshake failure and the workflow loads as unhealthy.

### 5. The `write-to-cosmos` workflow cannot run against the Linux Docker emulator

**The Logic Apps Standard CosmosDB service provider hardcodes Direct (RNTBD/TCP) connection mode.**
The Linux Docker emulator (`vnext-preview`) only supports Gateway (HTTP) mode and explicitly rejects
RNTBD address resolution with:
```
RNTBD protocol not supported for address resolution - use GATEWAY connection mode
```

There is no way to override this from the workflow, connection string (`connectionMode=Gateway` is
silently ignored), or app settings — the provider DLL has no configuration path for connection mode.

**Working alternatives for local Cosmos DB testing:**
- **Windows Cosmos DB Emulator** — supports Direct mode; run via `CosmosDB.Emulator.exe` or in a Windows VM
- **Real Azure Cosmos DB account** — use a free-tier account; update `COSMOS_CONNECTION_STRING` with the real connection string

The workflow file and connection definition are correct and will work against either of those targets.

---

### 6. No trailing slash on the endpoint

The Cosmos SDK constructs address-resolution URLs by appending `/addresses/` directly to the endpoint.
A trailing slash produces a double-slash (`//addresses/`) which the emulator rejects with 400 BadRequest.
```
# Wrong — causes 400 on every write
AccountEndpoint=http://localhost:8081/;AccountKey=...

# Correct
AccountEndpoint=http://localhost:8081;AccountKey=...
```
