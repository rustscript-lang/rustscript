using System.Diagnostics;
using System.Text;

namespace PdVm.Runtime;

internal static class PdVmBuiltinIo
{
    private static readonly Dictionary<long, IoHandle> Handles = new();
    private static readonly object Sync = new();
    private static readonly UTF8Encoding StrictUtf8 = new(false, true);
    private static long _nextHandle = 1;

    public static PdVmValue Open(string path, string mode)
    {
        var handle = CreateFileHandle(path, mode);
        return PdVmValue.FromInt(RegisterHandle(handle));
    }

    public static PdVmValue Popen(string command, string mode)
    {
        if (mode != "r" && mode != "w")
        {
            throw new InvalidOperationException($"unsupported io_popen mode '{mode}', expected r or w");
        }

        var startInfo = new ProcessStartInfo
        {
            FileName = OperatingSystem.IsWindows() ? "cmd.exe" : "/bin/sh",
            UseShellExecute = false,
            RedirectStandardInput = mode == "w",
            RedirectStandardOutput = mode == "r",
            RedirectStandardError = false,
        };

        if (OperatingSystem.IsWindows())
        {
            startInfo.ArgumentList.Add("/C");
            startInfo.ArgumentList.Add(command);
        }
        else
        {
            startInfo.ArgumentList.Add("-c");
            startInfo.ArgumentList.Add(command);
        }

        Process process;
        try
        {
            process = Process.Start(startInfo)
                ?? throw new InvalidOperationException("io_popen failed: process start returned null");
        }
        catch (Exception ex)
        {
            throw new InvalidOperationException($"io_popen failed: {ex.Message}", ex);
        }

        var handle = mode switch
        {
            "r" => new IoHandle(process.StandardOutput.BaseStream, process, readable: true, writable: false, appendWrites: false),
            "w" => new IoHandle(process.StandardInput.BaseStream, process, readable: false, writable: true, appendWrites: false),
            _ => throw new InvalidOperationException($"unsupported io_popen mode '{mode}', expected r or w"),
        };

        return PdVmValue.FromInt(RegisterHandle(handle));
    }

    public static PdVmValue ReadAll(long handleId)
    {
        var handle = GetHandle(handleId);
        if (!handle.Readable)
        {
            throw new InvalidOperationException("io_read_all requires a readable handle");
        }

        using var buffer = new MemoryStream();
        handle.Stream.CopyTo(buffer);
        return PdVmValue.FromString(DecodeStrictUtf8(buffer.ToArray(), "io_read_all"));
    }

    public static PdVmValue ReadLine(long handleId)
    {
        var handle = GetHandle(handleId);
        if (!handle.Readable)
        {
            throw new InvalidOperationException("io_read_line requires a readable handle");
        }

        var bytes = new List<byte>();
        while (true)
        {
            var next = handle.Stream.ReadByte();
            if (next < 0)
            {
                break;
            }

            bytes.Add((byte)next);
            if (next == '\n')
            {
                break;
            }
        }

        return PdVmValue.FromString(DecodeStrictUtf8(bytes.ToArray(), "io_read_line"));
    }

    public static PdVmValue Write(long handleId, string text)
    {
        var handle = GetHandle(handleId);
        if (!handle.Writable)
        {
            throw new InvalidOperationException("io_write requires a writable handle");
        }

        var bytes = StrictUtf8.GetBytes(text);
        if (handle.AppendWrites && handle.Stream.CanSeek)
        {
            handle.Stream.Seek(0, SeekOrigin.End);
        }

        handle.Stream.Write(bytes, 0, bytes.Length);
        return PdVmValue.FromInt(bytes.Length);
    }

    public static PdVmValue Flush(long handleId)
    {
        var handle = GetHandle(handleId);
        if (!handle.Writable && handle.Process is not null)
        {
            return PdVmValue.FromBool(true);
        }

        handle.Stream.Flush();
        return PdVmValue.FromBool(true);
    }

    public static PdVmValue Close(long handleId)
    {
        IoHandle handle;
        if (handleId <= 0)
        {
            throw new InvalidOperationException($"invalid io handle id {handleId}; expected positive handle id");
        }

        lock (Sync)
        {
            if (!Handles.Remove(handleId, out handle!))
            {
                throw new InvalidOperationException($"io handle {handleId} not found");
            }
        }

        handle.Dispose();
        return PdVmValue.FromBool(true);
    }

    public static PdVmValue Exists(string path) => PdVmValue.FromBool(Path.Exists(path));

    private static IoHandle CreateFileHandle(string path, string mode)
    {
        FileMode fileMode;
        FileAccess fileAccess;
        var appendWrites = false;

        switch (mode)
        {
            case "r":
                fileMode = FileMode.Open;
                fileAccess = FileAccess.Read;
                break;
            case "w":
                fileMode = FileMode.Create;
                fileAccess = FileAccess.Write;
                break;
            case "a":
                fileMode = FileMode.OpenOrCreate;
                fileAccess = FileAccess.Write;
                appendWrites = true;
                break;
            case "r+":
                fileMode = FileMode.Open;
                fileAccess = FileAccess.ReadWrite;
                break;
            case "w+":
                fileMode = FileMode.Create;
                fileAccess = FileAccess.ReadWrite;
                break;
            case "a+":
                fileMode = FileMode.OpenOrCreate;
                fileAccess = FileAccess.ReadWrite;
                appendWrites = true;
                break;
            default:
                throw new InvalidOperationException(
                    $"unsupported io_open mode '{mode}', expected r/w/a/r+/w+/a+");
        }

        var stream = new FileStream(path, fileMode, fileAccess, FileShare.Read);
        if (appendWrites && stream.CanSeek)
        {
            stream.Seek(0, SeekOrigin.End);
        }

        return new IoHandle(
            stream,
            process: null,
            readable: fileAccess.HasFlag(FileAccess.Read),
            writable: fileAccess.HasFlag(FileAccess.Write),
            appendWrites: appendWrites);
    }

    private static long RegisterHandle(IoHandle handle)
    {
        lock (Sync)
        {
            var handleId = _nextHandle++;
            Handles.Add(handleId, handle);
            return handleId;
        }
    }

    private static IoHandle GetHandle(long handleId)
    {
        if (handleId <= 0)
        {
            throw new InvalidOperationException($"invalid io handle id {handleId}; expected positive handle id");
        }

        lock (Sync)
        {
            if (!Handles.TryGetValue(handleId, out var handle))
            {
                throw new InvalidOperationException($"io handle {handleId} not found");
            }

            return handle;
        }
    }

    private static string DecodeStrictUtf8(byte[] bytes, string opName)
    {
        try
        {
            return StrictUtf8.GetString(bytes);
        }
        catch (DecoderFallbackException ex)
        {
            throw new InvalidOperationException($"{opName} failed: {ex.Message}", ex);
        }
    }

    private sealed class IoHandle(Stream stream, Process? process, bool readable, bool writable, bool appendWrites)
        : IDisposable
    {
        public Stream Stream { get; } = stream;

        public Process? Process { get; } = process;

        public bool Readable { get; } = readable;

        public bool Writable { get; } = writable;

        public bool AppendWrites { get; } = appendWrites;

        public void Dispose()
        {
            Stream.Dispose();
            if (Process is null)
            {
                return;
            }

            try
            {
                Process.WaitForExit();
            }
            finally
            {
                Process.Dispose();
            }
        }
    }
}
