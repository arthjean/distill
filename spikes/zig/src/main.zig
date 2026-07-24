const std = @import("std");
const c = @cImport({
    @cInclude("sqlite3.h");
    @cInclude("stdlib.h");
    @cInclude("sys/stat.h");
    @cInclude("time.h");
    @cInclude("unistd.h");
});

const Allocator = std.mem.Allocator;
const SCHEMA_VERSION = "distill.spike/v1";
const ARTIFACT_SCHEMA_VERSION = "distill.artifact/v1";
const PROJECTION_VERSION = "extract-lines/v1";
const POLICY_VERSION = "fixture-facts/v1";
const DEFAULT_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;
const DEFAULT_STORE_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_BUSY_TIMEOUT_MS: u64 = 250;
const MAX_INPUT_BYTES: usize = 10 * 1024 * 1024;
const MAX_JSON_LINE_BYTES: usize = 16 * 1024 * 1024;

const AppError = error{
    InvalidRequest,
    SchemaUnsupported,
    SourceUnsupported,
    TokenProfileUnsupported,
    BudgetUnsatisfiable,
    InputTooLarge,
    ResourceExhausted,
    UnsafeRoot,
    PermissionDenied,
    AcquisitionFailed,
    StoreFull,
    StoreBusy,
    CommitFailed,
    ArtifactUnknown,
    ArtifactExpired,
    ArtifactCorrupt,
    InvariantBreach,
    OutOfMemory,
};

const Operation = enum { project, recover, run, health };
const SourceKind = enum { @"inline", process };
const CountUnit = enum { bytes, tokens };
const FaultPoint = enum { before_insert, before_commit, after_commit, before_readback };

const Request = struct {
    schema_version: []const u8,
    request_id: []const u8,
    operation: Operation,
    store_path: []const u8,
    source: ?Source = null,
    artifact_id: ?[]const u8 = null,
    budget: ?Budget = null,
    retention: ?Retention = null,
    preservation: ?Preservation = null,
    max_store_bytes: ?u64 = null,
    busy_timeout_ms: ?u64 = null,
    fault_point: ?FaultPoint = null,
};

const Source = struct {
    kind: SourceKind,
    bytes_base64: ?[]const u8 = null,
    executable: ?[]const u8 = null,
    argv: []const []const u8 = &.{},
    cwd: ?[]const u8 = null,
    timeout_ms: ?u64 = null,
    output_limit: ?usize = null,
};

const Budget = struct {
    unit: CountUnit,
    total_visible_limit: usize,
    reserved_envelope: usize,
    token_profile: ?[]const u8 = null,
};

const Retention = struct {
    ttl_seconds: u64,
};

const Preservation = struct {
    profile: []const u8,
    p0: []const Fact,
    p1: []const Fact,
};

const Fact = struct {
    id: []const u8,
    needle_base64: []const u8,
};

const DecodedFact = struct {
    id: []const u8,
    needle: []const u8,
};

const DecodedFacts = struct {
    p0: []const DecodedFact,
    p1: []const DecodedFact,
};

const ArtifactRef = struct {
    schema_version: []const u8 = ARTIFACT_SCHEMA_VERSION,
    id: []const u8,
    source_sha256: []const u8,
    source_bytes: usize,
    created_at: u64,
    expires_at: u64,
};

const Span = struct {
    start: usize,
    end: usize,
};

const Projection = struct {
    visible: []const u8,
    fidelity: []const u8,
    retained_spans: []const Span,
    omitted_spans: []const Span,
    preserved_ids: []const []const u8,
};

const Event = struct {
    order: u8,
    stream: []const u8,
};

const AcquisitionReceipt = struct {
    variant: []const u8,
    stdout_sha256: ?[]const u8 = null,
    stderr_sha256: ?[]const u8 = null,
    events: ?[]const Event = null,
    exit_code: ?u8 = null,
    signal: ?u32 = null,
    timed_out: ?bool = null,
    working_directory: ?[]const u8 = null,
    truncated: ?bool = null,
};

const Acquired = struct {
    bytes: []const u8,
    receipt: AcquisitionReceipt,
};

const PreservationReceipt = struct {
    profile: []const u8,
    mandatory_fact_ids: []const []const u8,
};

const Receipt = struct {
    schema_version: []const u8 = "distill.receipt/v1",
    request_id: []const u8,
    source_sha256: []const u8,
    artifact: ArtifactRef,
    projection_version: []const u8 = PROJECTION_VERSION,
    policy_version: []const u8 = POLICY_VERSION,
    token_profile: ?[]const u8 = null,
    original_count: usize,
    visible_count: usize,
    count_unit: CountUnit = .bytes,
    fidelity: []const u8,
    retained_spans: []const Span,
    omitted_spans: []const Span,
    preservation: PreservationReceipt,
    acquisition: AcquisitionReceipt,
};

const Outcome = struct {
    candidate: ?[]const u8 = null,
    protocol: ?[]const u8 = null,
    runtime: ?[]const u8 = null,
    sqlite: ?[]const u8 = null,
    visible_base64: ?[]const u8 = null,
    artifact: ?ArtifactRef = null,
    receipt: ?Receipt = null,
    bytes_base64: ?[]const u8 = null,
};

const Failure = struct {
    code: []const u8,
    message: []const u8,
    request_id: ?[]const u8 = null,
    artifact: ?ArtifactRef = null,
};

const Response = struct {
    ok: bool,
    request_id: ?[]const u8 = null,
    outcome: ?Outcome = null,
    failure: ?Failure = null,
};

pub fn main(init: std.process.Init) !void {
    const stdin_buffer = try init.gpa.alloc(u8, MAX_JSON_LINE_BYTES + 1);
    defer init.gpa.free(stdin_buffer);
    var stdin_reader = std.Io.File.stdin().readerStreaming(init.io, stdin_buffer);
    var stdout_buffer: [4096]u8 = undefined;
    var stdout_writer = std.Io.File.stdout().writer(init.io, &stdout_buffer);
    const stdout = &stdout_writer.interface;

    while (true) {
        const maybe_line = stdin_reader.interface.takeDelimiter('\n') catch |err| switch (err) {
            error.StreamTooLong => {
                _ = stdin_reader.interface.discardDelimiterInclusive('\n') catch {};
                try emitResponse(stdout, Response{
                    .ok = false,
                    .failure = failure(error.InputTooLarge, null, null),
                });
                continue;
            },
            error.ReadFailed => return err,
        };
        const raw_line = maybe_line orelse break;
        const line = std.mem.trimEnd(u8, raw_line, "\r");
        if (line.len == 0) continue;
        var arena = std.heap.ArenaAllocator.init(init.gpa);
        defer arena.deinit();
        const allocator = arena.allocator();

        const parsed = std.json.parseFromSliceLeaky(Request, allocator, line, .{
            .ignore_unknown_fields = false,
        }) catch {
            try emitResponse(stdout, Response{
                .ok = false,
                .failure = failure(error.InvalidRequest, null, null),
            });
            continue;
        };
        const response = handle(init, allocator, parsed);
        try emitResponse(stdout, response);
    }
    try stdout.flush();
}

fn emitResponse(writer: *std.Io.Writer, response: Response) !void {
    try std.json.Stringify.value(response, .{}, writer);
    try writer.writeByte('\n');
}

fn handle(init: std.process.Init, allocator: Allocator, request: Request) Response {
    validateRequest(request) catch |err| {
        return .{
            .ok = false,
            .failure = failure(err, request.request_id, null),
        };
    };

    const outcome = switch (request.operation) {
        .health => healthOutcome(allocator) catch |err| {
            return .{
                .ok = false,
                .failure = failure(err, request.request_id, null),
            };
        },
        .recover => recoverOutcome(allocator, request) catch |err| {
            return .{
                .ok = false,
                .failure = failure(err, request.request_id, null),
            };
        },
        .project, .run => projectOutcome(init, allocator, request) catch |err| {
            return .{
                .ok = false,
                .failure = failure(err, request.request_id, pending_artifact),
            };
        },
    };
    return .{
        .ok = true,
        .request_id = request.request_id,
        .outcome = outcome,
    };
}

threadlocal var pending_artifact: ?ArtifactRef = null;

fn validateRequest(request: Request) AppError!void {
    if (!std.mem.eql(u8, request.schema_version, SCHEMA_VERSION)) {
        return error.SchemaUnsupported;
    }
    if (request.request_id.len == 0 or request.request_id.len > 128) {
        return error.InvalidRequest;
    }
    if ((request.busy_timeout_ms orelse DEFAULT_BUSY_TIMEOUT_MS) > 5000) {
        return error.InvalidRequest;
    }
}

fn healthOutcome(allocator: Allocator) AppError!Outcome {
    const sqlite_version = c.sqlite3_libversion();
    if (sqlite_version == null) return error.InvariantBreach;
    return .{
        .candidate = "zig",
        .protocol = SCHEMA_VERSION,
        .runtime = @import("builtin").zig_version_string,
        .sqlite = try allocator.dupe(u8, std.mem.span(sqlite_version)),
    };
}

fn projectOutcome(
    init: std.process.Init,
    allocator: Allocator,
    request: Request,
) AppError!Outcome {
    pending_artifact = null;
    const budget = request.budget orelse return error.InvalidRequest;
    if (budget.reserved_envelope > budget.total_visible_limit) {
        return error.InvalidRequest;
    }
    if (budget.unit == .tokens) {
        _ = budget.token_profile;
        return error.TokenProfileUnsupported;
    }
    const preservation = request.preservation orelse return error.InvalidRequest;
    if (!std.mem.eql(u8, preservation.profile, POLICY_VERSION)) {
        return error.InvalidRequest;
    }
    const facts = try decodeFacts(allocator, preservation);
    const acquired = try acquire(init, allocator, request);
    if (acquired.bytes.len > MAX_INPUT_BYTES) return error.InputTooLarge;

    const db = try openStore(
        allocator,
        request.store_path,
        request.busy_timeout_ms orelse DEFAULT_BUSY_TIMEOUT_MS,
    );
    defer _ = c.sqlite3_close(db);
    const artifact = try capture(
        allocator,
        db,
        request.store_path,
        acquired.bytes,
        if (request.retention) |retention| retention.ttl_seconds else DEFAULT_TTL_SECONDS,
        request.max_store_bytes orelse DEFAULT_STORE_BYTES,
        request.fault_point,
    );
    pending_artifact = artifact;

    const projection = try makeProjection(
        allocator,
        acquired.bytes,
        budget.total_visible_limit - budget.reserved_envelope,
        facts,
    );
    const visible_base64 = try encodeBase64(allocator, projection.visible);
    const receipt = Receipt{
        .request_id = request.request_id,
        .source_sha256 = artifact.source_sha256,
        .artifact = artifact,
        .original_count = acquired.bytes.len,
        .visible_count = projection.visible.len,
        .fidelity = projection.fidelity,
        .retained_spans = projection.retained_spans,
        .omitted_spans = projection.omitted_spans,
        .preservation = .{
            .profile = POLICY_VERSION,
            .mandatory_fact_ids = projection.preserved_ids,
        },
        .acquisition = acquired.receipt,
    };
    pending_artifact = null;
    return .{
        .visible_base64 = visible_base64,
        .artifact = artifact,
        .receipt = receipt,
    };
}

fn recoverOutcome(allocator: Allocator, request: Request) AppError!Outcome {
    const id = request.artifact_id orelse return error.InvalidRequest;
    if (!isArtifactId(id)) return error.InvalidRequest;
    const db = try openStore(
        allocator,
        request.store_path,
        request.busy_timeout_ms orelse DEFAULT_BUSY_TIMEOUT_MS,
    );
    defer _ = c.sqlite3_close(db);
    const recovered = try recover(allocator, db, id);
    return .{
        .artifact = recovered.artifact,
        .bytes_base64 = try encodeBase64(allocator, recovered.bytes),
    };
}

fn acquire(
    init: std.process.Init,
    allocator: Allocator,
    request: Request,
) AppError!Acquired {
    const source = request.source orelse return error.InvalidRequest;
    switch (request.operation) {
        .project => {
            if (source.kind != .@"inline") return error.SourceUnsupported;
            const encoded = source.bytes_base64 orelse return error.InvalidRequest;
            return .{
                .bytes = try decodeBase64(allocator, encoded),
                .receipt = .{ .variant = "inline" },
            };
        },
        .run => {
            if (source.kind != .process) return error.SourceUnsupported;
            return acquireProcess(init, allocator, source);
        },
        else => return error.SourceUnsupported,
    }
}

fn acquireProcess(
    init: std.process.Init,
    allocator: Allocator,
    source: Source,
) AppError!Acquired {
    const executable = source.executable orelse return error.InvalidRequest;
    const cwd = source.cwd orelse return error.InvalidRequest;
    const timeout_ms = source.timeout_ms orelse return error.InvalidRequest;
    const output_limit = source.output_limit orelse return error.InvalidRequest;
    if (executable.len == 0 or source.argv.len > 64 or timeout_ms == 0 or
        timeout_ms > 30_000 or output_limit == 0 or output_limit > MAX_INPUT_BYTES)
    {
        return error.InvalidRequest;
    }

    var argv = try allocator.alloc([]const u8, source.argv.len + 1);
    argv[0] = executable;
    @memcpy(argv[1..], source.argv);
    var empty_environment = std.process.Environ.Map.init(allocator);
    defer empty_environment.deinit();
    const timeout = (std.Io.Timeout{ .duration = .{
        .raw = std.Io.Duration.fromMilliseconds(@intCast(timeout_ms)),
        .clock = .awake,
    } }).toDeadline(init.io);
    const result = try runProcessGroup(
        allocator,
        init.io,
        argv,
        cwd,
        &empty_environment,
        output_limit,
        timeout,
    );
    if (result.stdout.len + result.stderr.len > output_limit) {
        return error.ResourceExhausted;
    }
    const exit_code: u8 = switch (result.term) {
        .exited => |code| code,
        else => return error.AcquisitionFailed,
    };
    const stdout_sha256 = try sha256Hex(allocator, result.stdout);
    const stderr_sha256 = try sha256Hex(allocator, result.stderr);
    const bytes = try allocator.alloc(u8, result.stdout.len + result.stderr.len);
    @memcpy(bytes[0..result.stdout.len], result.stdout);
    @memcpy(bytes[result.stdout.len..], result.stderr);
    const events = try allocator.dupe(Event, &.{
        .{ .order = 0, .stream = "stdout" },
        .{ .order = 1, .stream = "stderr" },
    });
    return .{
        .bytes = bytes,
        .receipt = .{
            .variant = "process",
            .stdout_sha256 = stdout_sha256,
            .stderr_sha256 = stderr_sha256,
            .events = events,
            .exit_code = exit_code,
            .signal = null,
            .timed_out = false,
            .working_directory = cwd,
            .truncated = false,
        },
    };
}

fn runProcessGroup(
    allocator: Allocator,
    io: std.Io,
    argv: []const []const u8,
    cwd: []const u8,
    environment: *const std.process.Environ.Map,
    output_limit: usize,
    timeout: std.Io.Timeout,
) AppError!std.process.RunResult {
    var child = std.process.spawn(io, .{
        .argv = argv,
        .cwd = .{ .path = cwd },
        .environ_map = environment,
        .pgid = 0,
        .stdin = .ignore,
        .stdout = .pipe,
        .stderr = .pipe,
    }) catch return error.AcquisitionFailed;
    const process_group = child.id orelse return error.AcquisitionFailed;
    defer terminateProcessGroup(&child, io, process_group);

    var multi_reader_buffer: std.Io.File.MultiReader.Buffer(2) = undefined;
    var multi_reader: std.Io.File.MultiReader = undefined;
    multi_reader.init(
        allocator,
        io,
        multi_reader_buffer.toStreams(),
        &.{ child.stdout.?, child.stderr.? },
    );
    defer multi_reader.deinit();
    const stdout_reader = multi_reader.reader(0);
    const stderr_reader = multi_reader.reader(1);

    while (multi_reader.fill(64, timeout)) |_| {
        if (stdout_reader.buffered().len > output_limit or
            stderr_reader.buffered().len > output_limit)
        {
            return error.ResourceExhausted;
        }
    } else |err| switch (err) {
        error.EndOfStream => {},
        error.Timeout => return error.AcquisitionFailed,
        else => return error.AcquisitionFailed,
    }
    multi_reader.checkAnyError() catch return error.AcquisitionFailed;
    const term = try waitForChild(&child, io, timeout);
    const stdout = multi_reader.toOwnedSlice(0) catch return error.OutOfMemory;
    const stderr = multi_reader.toOwnedSlice(1) catch return error.OutOfMemory;
    return .{ .term = term, .stdout = stdout, .stderr = stderr };
}

fn waitForChild(
    child: *std.process.Child,
    io: std.Io,
    timeout: std.Io.Timeout,
) AppError!std.process.Child.Term {
    const WaitResult = union(enum) {
        child: std.process.Child.WaitError!std.process.Child.Term,
        timeout: std.Io.Cancelable!void,
    };
    var result_buffer: [2]WaitResult = undefined;
    var select = std.Io.Select(WaitResult).init(io, &result_buffer);
    select.async(.child, std.process.Child.wait, .{ child, io });
    select.async(.timeout, std.Io.Timeout.sleep, .{ timeout, io });
    const selected = select.await() catch {
        select.cancelDiscard();
        return error.AcquisitionFailed;
    };
    select.cancelDiscard();
    return switch (selected) {
        .child => |result| result catch error.AcquisitionFailed,
        .timeout => error.AcquisitionFailed,
    };
}

fn terminateProcessGroup(
    child: *std.process.Child,
    io: std.Io,
    process_group: std.posix.pid_t,
) void {
    std.posix.kill(-process_group, .KILL) catch {};
    child.kill(io);
}

fn decodeFacts(allocator: Allocator, preservation: Preservation) AppError!DecodedFacts {
    return .{
        .p0 = try decodeFactGroup(allocator, preservation.p0),
        .p1 = try decodeFactGroup(allocator, preservation.p1),
    };
}

fn decodeFactGroup(allocator: Allocator, facts: []const Fact) AppError![]const DecodedFact {
    if (facts.len > 64) return error.InvalidRequest;
    const decoded = try allocator.alloc(DecodedFact, facts.len);
    for (facts, 0..) |fact, index| {
        if (fact.id.len == 0 or fact.id.len > 128) return error.InvalidRequest;
        const needle = try decodeBase64(allocator, fact.needle_base64);
        if (needle.len == 0) return error.InvalidRequest;
        decoded[index] = .{ .id = fact.id, .needle = needle };
    }
    return decoded;
}

fn decodeBase64(allocator: Allocator, encoded: []const u8) AppError![]const u8 {
    const size = std.base64.standard.Decoder.calcSizeForSlice(encoded) catch {
        return error.InvalidRequest;
    };
    const bytes = try allocator.alloc(u8, size);
    std.base64.standard.Decoder.decode(bytes, encoded) catch {
        return error.InvalidRequest;
    };
    const canonical = try encodeBase64(allocator, bytes);
    if (!std.mem.eql(u8, canonical, encoded)) return error.InvalidRequest;
    return bytes;
}

fn encodeBase64(allocator: Allocator, bytes: []const u8) AppError![]const u8 {
    const size = std.base64.standard.Encoder.calcSize(bytes.len);
    const encoded = try allocator.alloc(u8, size);
    return std.base64.standard.Encoder.encode(encoded, bytes);
}

fn makeProjection(
    allocator: Allocator,
    source: []const u8,
    limit: usize,
    facts: DecodedFacts,
) AppError!Projection {
    for (facts.p0) |fact| {
        if (std.mem.indexOf(u8, source, fact.needle) == null) {
            return error.InvariantBreach;
        }
    }
    if (std.unicode.utf8ValidateSlice(source)) {
        if (source.len <= limit) {
            const retained = try allocator.dupe(Span, &.{.{ .start = 0, .end = source.len }});
            const ids = try allocator.alloc([]const u8, facts.p0.len);
            for (facts.p0, 0..) |fact, index| ids[index] = fact.id;
            return .{
                .visible = source,
                .fidelity = "exact",
                .retained_spans = retained,
                .omitted_spans = &.{},
                .preserved_ids = ids,
            };
        }
        return extractProjection(allocator, source, limit, facts);
    }
    if (facts.p0.len != 0) return error.BudgetUnsatisfiable;
    const encoded = try encodeBase64(allocator, source);
    const visible = try std.fmt.allocPrint(allocator, "base64:{s}", .{encoded});
    if (visible.len > limit) return error.BudgetUnsatisfiable;
    return .{
        .visible = visible,
        .fidelity = "encoded",
        .retained_spans = &.{},
        .omitted_spans = try allocator.dupe(Span, &.{.{ .start = 0, .end = source.len }}),
        .preserved_ids = &.{},
    };
}

fn extractProjection(
    allocator: Allocator,
    source: []const u8,
    limit: usize,
    facts: DecodedFacts,
) AppError!Projection {
    var spans: std.ArrayList(Span) = .empty;
    var ids: std.ArrayList([]const u8) = .empty;
    for (facts.p0) |fact| {
        const position = std.mem.indexOf(u8, source, fact.needle) orelse {
            return error.InvariantBreach;
        };
        try spans.append(allocator, lineSpan(source, position, position + fact.needle.len));
        try ids.append(allocator, fact.id);
    }
    mergeSpans(&spans);
    var rendered = try renderSpans(allocator, source, spans.items);
    if (rendered.len > limit) return error.BudgetUnsatisfiable;

    for (facts.p1) |fact| {
        const position = std.mem.indexOf(u8, source, fact.needle) orelse continue;
        var candidate = try spans.clone(allocator);
        try candidate.append(allocator, lineSpan(source, position, position + fact.needle.len));
        mergeSpans(&candidate);
        const candidate_rendered = try renderSpans(allocator, source, candidate.items);
        if (candidate_rendered.len <= limit) {
            spans = candidate;
            rendered = candidate_rendered;
        }
    }
    if (spans.items.len == 0) {
        const prefix_end = utf8PrefixLen(source, limit);
        if (prefix_end > 0) try spans.append(allocator, .{ .start = 0, .end = prefix_end });
        rendered = try renderSpans(allocator, source, spans.items);
    }
    if (rendered.len > limit) return error.BudgetUnsatisfiable;
    const retained_spans = try allocator.dupe(Span, spans.items);
    const omitted_spans = try complementSpans(allocator, source.len, spans.items);
    const preserved_ids = try ids.toOwnedSlice(allocator);
    return .{
        .visible = rendered,
        .fidelity = "extractive",
        .retained_spans = retained_spans,
        .omitted_spans = omitted_spans,
        .preserved_ids = preserved_ids,
    };
}

fn lineSpan(source: []const u8, start: usize, end: usize) Span {
    var line_start = start;
    while (line_start > 0 and source[line_start - 1] != '\n') : (line_start -= 1) {}
    var line_end = end;
    while (line_end < source.len and source[line_end] != '\n') : (line_end += 1) {}
    if (line_end < source.len) line_end += 1;
    return .{ .start = line_start, .end = line_end };
}

fn mergeSpans(spans: *std.ArrayList(Span)) void {
    std.mem.sort(Span, spans.items, {}, struct {
        fn lessThan(_: void, left: Span, right: Span) bool {
            return left.start < right.start;
        }
    }.lessThan);
    if (spans.items.len < 2) return;
    var write_index: usize = 0;
    for (spans.items[1..]) |span| {
        if (span.start <= spans.items[write_index].end) {
            spans.items[write_index].end = @max(spans.items[write_index].end, span.end);
        } else {
            write_index += 1;
            spans.items[write_index] = span;
        }
    }
    spans.shrinkRetainingCapacity(write_index + 1);
}

fn renderSpans(allocator: Allocator, source: []const u8, spans: []const Span) AppError![]const u8 {
    var visible: std.ArrayList(u8) = .empty;
    var cursor: usize = 0;
    for (spans) |span| {
        if (span.start > cursor) try appendOmission(allocator, &visible, span.start - cursor);
        try visible.appendSlice(allocator, source[span.start..span.end]);
        cursor = span.end;
    }
    if (cursor < source.len) try appendOmission(allocator, &visible, source.len - cursor);
    return visible.toOwnedSlice(allocator);
}

fn appendOmission(
    allocator: Allocator,
    visible: *std.ArrayList(u8),
    omitted: usize,
) AppError!void {
    const marker = try std.fmt.allocPrint(allocator, "[... omitted {d} bytes ...]\n", .{omitted});
    try visible.appendSlice(allocator, marker);
}

fn complementSpans(
    allocator: Allocator,
    source_len: usize,
    spans: []const Span,
) AppError![]const Span {
    var omitted: std.ArrayList(Span) = .empty;
    var cursor: usize = 0;
    for (spans) |span| {
        if (span.start > cursor) {
            try omitted.append(allocator, .{ .start = cursor, .end = span.start });
        }
        cursor = span.end;
    }
    if (cursor < source_len) {
        try omitted.append(allocator, .{ .start = cursor, .end = source_len });
    }
    return omitted.toOwnedSlice(allocator);
}

fn utf8PrefixLen(source: []const u8, limit: usize) usize {
    var end = @min(source.len, limit);
    while (end > 0 and !std.unicode.utf8ValidateSlice(source[0..end])) : (end -= 1) {}
    return end;
}

fn openStore(
    allocator: Allocator,
    path: []const u8,
    busy_timeout_ms: u64,
) AppError!*c.sqlite3 {
    const parent = std.fs.path.dirname(path) orelse return error.UnsafeRoot;
    try chmodPath(allocator, parent, 0o700);
    const path_z = try allocator.dupeZ(u8, path);
    var db_optional: ?*c.sqlite3 = null;
    const open_result = c.sqlite3_open_v2(
        path_z.ptr,
        &db_optional,
        c.SQLITE_OPEN_READWRITE | c.SQLITE_OPEN_CREATE | c.SQLITE_OPEN_FULLMUTEX,
        null,
    );
    if (open_result != c.SQLITE_OK) {
        if (db_optional) |failed_db| _ = c.sqlite3_close(failed_db);
        return mapSqliteCode(open_result);
    }
    const db = db_optional orelse return error.CommitFailed;
    errdefer _ = c.sqlite3_close(db);
    if (c.sqlite3_busy_timeout(db, @intCast(busy_timeout_ms)) != c.SQLITE_OK) {
        return error.CommitFailed;
    }
    try execSql(db, "PRAGMA journal_mode=WAL;");
    try execSql(db, "PRAGMA synchronous=FULL;");
    try execSql(db,
        \\CREATE TABLE IF NOT EXISTS artifacts (
        \\ id TEXT PRIMARY KEY,
        \\ source BLOB NOT NULL,
        \\ sha256 TEXT NOT NULL,
        \\ source_bytes INTEGER NOT NULL,
        \\ created_at INTEGER NOT NULL,
        \\ expires_at INTEGER NOT NULL
        \\);
    );
    try chmodStoreFiles(allocator, path);
    return db;
}

fn capture(
    allocator: Allocator,
    db: *c.sqlite3,
    path: []const u8,
    source: []const u8,
    ttl_seconds: u64,
    max_store_bytes: u64,
    fault_point: ?FaultPoint,
) AppError!ArtifactRef {
    if (ttl_seconds == 0 or ttl_seconds > 30 * 24 * 60 * 60) {
        return error.InvalidRequest;
    }
    const digest = try sha256Hex(allocator, source);
    const created_at = unixSeconds();
    const expires_at = created_at +| ttl_seconds;
    try execSql(db, "BEGIN IMMEDIATE;");
    errdefer execSql(db, "ROLLBACK;") catch {};

    const used = try queryInt(db, "SELECT COALESCE(SUM(source_bytes), 0) FROM artifacts;");
    if (used < 0 or @as(u64, @intCast(used)) +| @as(u64, @intCast(source.len)) > max_store_bytes) {
        return error.StoreFull;
    }
    const id = try queryText(allocator, db, "SELECT lower(hex(randomblob(16)));");
    maybeFault(fault_point, .before_insert);

    var statement_optional: ?*c.sqlite3_stmt = null;
    const prepare_result = c.sqlite3_prepare_v2(
        db,
        "INSERT INTO artifacts (id, source, sha256, source_bytes, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6);",
        -1,
        &statement_optional,
        null,
    );
    if (prepare_result != c.SQLITE_OK) return mapSqliteCode(prepare_result);
    const statement = statement_optional orelse return error.CommitFailed;
    defer _ = c.sqlite3_finalize(statement);
    if (c.sqlite3_bind_text(statement, 1, id.ptr, @intCast(id.len), c.SQLITE_TRANSIENT) != c.SQLITE_OK or
        c.sqlite3_bind_blob(statement, 2, source.ptr, @intCast(source.len), c.SQLITE_TRANSIENT) != c.SQLITE_OK or
        c.sqlite3_bind_text(statement, 3, digest.ptr, @intCast(digest.len), c.SQLITE_TRANSIENT) != c.SQLITE_OK or
        c.sqlite3_bind_int64(statement, 4, @intCast(source.len)) != c.SQLITE_OK or
        c.sqlite3_bind_int64(statement, 5, @intCast(created_at)) != c.SQLITE_OK or
        c.sqlite3_bind_int64(statement, 6, @intCast(expires_at)) != c.SQLITE_OK)
    {
        return error.CommitFailed;
    }
    const step_result = c.sqlite3_step(statement);
    if (step_result != c.SQLITE_DONE) return mapSqliteCode(step_result);
    maybeFault(fault_point, .before_commit);
    try execSql(db, "COMMIT;");
    maybeFault(fault_point, .after_commit);
    try chmodStoreFiles(allocator, path);
    maybeFault(fault_point, .before_readback);

    const recovered = try recover(allocator, db, id);
    if (!std.mem.eql(u8, recovered.artifact.source_sha256, digest) or
        !std.mem.eql(u8, recovered.bytes, source))
    {
        return error.CommitFailed;
    }
    return .{
        .id = id,
        .source_sha256 = digest,
        .source_bytes = source.len,
        .created_at = created_at,
        .expires_at = expires_at,
    };
}

const Recovered = struct {
    artifact: ArtifactRef,
    bytes: []const u8,
};

fn recover(allocator: Allocator, db: *c.sqlite3, id: []const u8) AppError!Recovered {
    var statement_optional: ?*c.sqlite3_stmt = null;
    const prepare_result = c.sqlite3_prepare_v2(
        db,
        "SELECT source, sha256, source_bytes, created_at, expires_at FROM artifacts WHERE id = ?1;",
        -1,
        &statement_optional,
        null,
    );
    if (prepare_result != c.SQLITE_OK) return mapSqliteCode(prepare_result);
    const statement = statement_optional orelse return error.CommitFailed;
    defer _ = c.sqlite3_finalize(statement);
    if (c.sqlite3_bind_text(statement, 1, id.ptr, @intCast(id.len), c.SQLITE_TRANSIENT) != c.SQLITE_OK) {
        return error.CommitFailed;
    }
    const step_result = c.sqlite3_step(statement);
    if (step_result == c.SQLITE_DONE) return error.ArtifactUnknown;
    if (step_result != c.SQLITE_ROW) return mapSqliteCode(step_result);

    const blob_length = c.sqlite3_column_bytes(statement, 0);
    if (blob_length < 0) return error.ArtifactCorrupt;
    const blob_ptr = c.sqlite3_column_blob(statement, 0);
    if (blob_length > 0 and blob_ptr == null) return error.ArtifactCorrupt;
    const blob: []const u8 = if (blob_length == 0)
        &.{}
    else
        @as([*]const u8, @ptrCast(blob_ptr.?))[0..@intCast(blob_length)];
    const bytes = try allocator.dupe(u8, blob);
    const digest_ptr = c.sqlite3_column_text(statement, 1) orelse return error.ArtifactCorrupt;
    const digest_length = c.sqlite3_column_bytes(statement, 1);
    const digest = try allocator.dupe(
        u8,
        @as([*]const u8, @ptrCast(digest_ptr))[0..@intCast(digest_length)],
    );
    const source_bytes = c.sqlite3_column_int64(statement, 2);
    const created_at = c.sqlite3_column_int64(statement, 3);
    const expires_at = c.sqlite3_column_int64(statement, 4);
    if (source_bytes < 0 or created_at < 0 or expires_at < 0) return error.ArtifactCorrupt;
    const artifact = ArtifactRef{
        .id = id,
        .source_sha256 = digest,
        .source_bytes = @intCast(source_bytes),
        .created_at = @intCast(created_at),
        .expires_at = @intCast(expires_at),
    };
    if (unixSeconds() >= artifact.expires_at) return error.ArtifactExpired;
    const actual_digest = try sha256Hex(allocator, bytes);
    if (artifact.source_bytes != bytes.len or !std.mem.eql(u8, digest, actual_digest)) {
        return error.ArtifactCorrupt;
    }
    return .{ .artifact = artifact, .bytes = bytes };
}

fn execSql(db: *c.sqlite3, sql: [*:0]const u8) AppError!void {
    const result = c.sqlite3_exec(db, sql, null, null, null);
    if (result != c.SQLITE_OK) return mapSqliteCode(result);
}

fn queryInt(db: *c.sqlite3, sql: [*:0]const u8) AppError!i64 {
    var statement_optional: ?*c.sqlite3_stmt = null;
    const prepare_result = c.sqlite3_prepare_v2(db, sql, -1, &statement_optional, null);
    if (prepare_result != c.SQLITE_OK) return mapSqliteCode(prepare_result);
    const statement = statement_optional orelse return error.CommitFailed;
    defer _ = c.sqlite3_finalize(statement);
    const step_result = c.sqlite3_step(statement);
    if (step_result != c.SQLITE_ROW) return mapSqliteCode(step_result);
    return c.sqlite3_column_int64(statement, 0);
}

fn queryText(
    allocator: Allocator,
    db: *c.sqlite3,
    sql: [*:0]const u8,
) AppError![]const u8 {
    var statement_optional: ?*c.sqlite3_stmt = null;
    const prepare_result = c.sqlite3_prepare_v2(db, sql, -1, &statement_optional, null);
    if (prepare_result != c.SQLITE_OK) return mapSqliteCode(prepare_result);
    const statement = statement_optional orelse return error.CommitFailed;
    defer _ = c.sqlite3_finalize(statement);
    const step_result = c.sqlite3_step(statement);
    if (step_result != c.SQLITE_ROW) return mapSqliteCode(step_result);
    const value = c.sqlite3_column_text(statement, 0) orelse return error.CommitFailed;
    const length = c.sqlite3_column_bytes(statement, 0);
    return allocator.dupe(u8, @as([*]const u8, @ptrCast(value))[0..@intCast(length)]);
}

fn mapSqliteCode(code: c_int) AppError {
    return switch (code & 0xff) {
        c.SQLITE_BUSY, c.SQLITE_LOCKED => error.StoreBusy,
        c.SQLITE_FULL => error.StoreFull,
        c.SQLITE_PERM, c.SQLITE_READONLY => error.PermissionDenied,
        c.SQLITE_CORRUPT, c.SQLITE_NOTADB => error.ArtifactCorrupt,
        else => error.CommitFailed,
    };
}

fn maybeFault(configured: ?FaultPoint, current: FaultPoint) void {
    if (configured == null or configured.? != current) return;
    const enabled = c.getenv("DISTILL_SPIKE_ENABLE_FAULTS") orelse return;
    if (std.mem.eql(u8, std.mem.span(enabled), "1")) std.process.exit(86);
}

fn chmodPath(allocator: Allocator, path: []const u8, mode: c.mode_t) AppError!void {
    const path_z = try allocator.dupeZ(u8, path);
    if (c.chmod(path_z.ptr, mode) != 0) return error.PermissionDenied;
}

fn chmodStoreFiles(allocator: Allocator, path: []const u8) AppError!void {
    try chmodPath(allocator, path, 0o600);
    const wal = try std.fmt.allocPrint(allocator, "{s}-wal", .{path});
    const shm = try std.fmt.allocPrint(allocator, "{s}-shm", .{path});
    try chmodOptional(allocator, wal, 0o600);
    try chmodOptional(allocator, shm, 0o600);
}

fn chmodOptional(allocator: Allocator, path: []const u8, mode: c.mode_t) AppError!void {
    const path_z = try allocator.dupeZ(u8, path);
    if (c.access(path_z.ptr, c.F_OK) != 0) return;
    if (c.chmod(path_z.ptr, mode) != 0) return error.PermissionDenied;
}

fn sha256Hex(allocator: Allocator, bytes: []const u8) AppError![]const u8 {
    var digest: [std.crypto.hash.sha2.Sha256.digest_length]u8 = undefined;
    std.crypto.hash.sha2.Sha256.hash(bytes, &digest, .{});
    const hex = std.fmt.bytesToHex(digest, .lower);
    return allocator.dupe(u8, &hex);
}

fn unixSeconds() u64 {
    const timestamp = c.time(null);
    return if (timestamp < 0) 0 else @intCast(timestamp);
}

fn isArtifactId(id: []const u8) bool {
    if (id.len != 32) return false;
    for (id) |byte| {
        if (!std.ascii.isHex(byte) or std.ascii.isUpper(byte)) return false;
    }
    return true;
}

fn failure(err: AppError, request_id: ?[]const u8, artifact: ?ArtifactRef) Failure {
    return .{
        .code = errorCode(err),
        .message = errorMessage(err),
        .request_id = request_id,
        .artifact = artifact,
    };
}

fn errorCode(err: AppError) []const u8 {
    return switch (err) {
        error.InvalidRequest => "invalid_request",
        error.SchemaUnsupported => "schema_unsupported",
        error.SourceUnsupported => "source_unsupported",
        error.TokenProfileUnsupported => "token_profile_unsupported",
        error.BudgetUnsatisfiable => "budget_unsatisfiable",
        error.InputTooLarge => "input_too_large",
        error.ResourceExhausted => "resource_exhausted",
        error.UnsafeRoot => "unsafe_root",
        error.PermissionDenied => "permission_denied",
        error.AcquisitionFailed => "acquisition_failed",
        error.StoreFull => "store_full",
        error.StoreBusy => "store_busy",
        error.CommitFailed => "commit_failed",
        error.ArtifactUnknown => "artifact_unknown",
        error.ArtifactExpired => "artifact_expired",
        error.ArtifactCorrupt => "artifact_corrupt",
        error.InvariantBreach => "invariant_breach",
        error.OutOfMemory => "resource_exhausted",
    };
}

fn errorMessage(err: AppError) []const u8 {
    return switch (err) {
        error.InvalidRequest => "request is invalid",
        error.SchemaUnsupported => "unsupported spike schema version",
        error.SourceUnsupported => "operation and source kind do not match",
        error.TokenProfileUnsupported => "the spike does not implement a versioned tokenizer",
        error.BudgetUnsatisfiable => "mandatory content exceeds the visible budget",
        error.InputTooLarge => "source exceeds the 10 MiB spike limit",
        error.ResourceExhausted => "configured resource limit was exceeded",
        error.UnsafeRoot => "store path must have a parent directory",
        error.PermissionDenied => "private store permissions cannot be enforced",
        error.AcquisitionFailed => "process acquisition failed",
        error.StoreFull => "artifact store byte cap would be exceeded",
        error.StoreBusy => "artifact transaction timed out",
        error.CommitFailed => "artifact transaction failed",
        error.ArtifactUnknown => "artifact does not exist",
        error.ArtifactExpired => "artifact retention expired",
        error.ArtifactCorrupt => "artifact digest mismatch",
        error.InvariantBreach => "projection invariant failed",
        error.OutOfMemory => "allocation failed",
    };
}

test "projection is bounded and preserves mandatory facts" {
    var arena = std.heap.ArenaAllocator.init(std.testing.allocator);
    defer arena.deinit();
    const allocator = arena.allocator();
    const source = "header\nnoise noise noise\nFATAL fact\nmore noise\nhelpful hint\ntail\n";
    const projection = try makeProjection(allocator, source, 80, .{
        .p0 = &.{.{ .id = "fatal", .needle = "FATAL fact" }},
        .p1 = &.{.{ .id = "hint", .needle = "helpful hint" }},
    });
    try std.testing.expect(projection.visible.len <= 80);
    try std.testing.expect(std.mem.indexOf(u8, projection.visible, "FATAL fact") != null);
}
