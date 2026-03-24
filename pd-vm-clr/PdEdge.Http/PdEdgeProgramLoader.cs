using System.Diagnostics;
using System.Reflection;
using PdVm.Compiler;
using PdVm.Runtime;

namespace PdEdge.Http;

public sealed class PdEdgeLoadedProgram
{
    public required IReadOnlyList<PdVmHostImport> Imports { get; init; }

    public required bool UsesAsyncHostOps { get; init; }

    public required Func<IPdVmProgram> CreateProgram { get; init; }
}

public static class PdEdgeProgramLoader
{
    public static async Task<PdEdgeLoadedProgram?> LoadAsync(
        PdEdgeHttpOptions options,
        CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(options);

        if (string.IsNullOrWhiteSpace(options.ProgramSourcePath) &&
            string.IsNullOrWhiteSpace(options.ProgramVmbcPath))
        {
            return null;
        }

        if (!string.IsNullOrWhiteSpace(options.ProgramSourcePath))
        {
            return await LoadFromSourceFileAsync(options.ProgramSourcePath!, cancellationToken);
        }

        return await LoadFromVmbcFileAsync(options.ProgramVmbcPath!, cancellationToken);
    }

    public static async Task<PdEdgeLoadedProgram> LoadFromSourceFileAsync(
        string sourcePath,
        CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(sourcePath);

        var tempRoot = GetTempWorkRoot();
        Directory.CreateDirectory(tempRoot);
        var tempVmbc = Path.Combine(tempRoot, $"{Guid.NewGuid():N}.vmbc");
        try
        {
            await CompileSourceFileToVmbcAsync(sourcePath, tempVmbc, cancellationToken);
            return await LoadFromVmbcFileAsync(tempVmbc, cancellationToken);
        }
        finally
        {
            TryDelete(tempVmbc);
        }
    }

    public static async Task<PdEdgeLoadedProgram> LoadFromVmbcFileAsync(
        string vmbcPath,
        CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(vmbcPath);
        var bytes = await File.ReadAllBytesAsync(vmbcPath, cancellationToken);
        return LoadFromVmbcBytes(bytes, Path.GetFileNameWithoutExtension(vmbcPath));
    }

    public static PdEdgeLoadedProgram LoadFromVmbcBytes(byte[] vmbcBytes, string? assemblyStem = null)
    {
        ArgumentNullException.ThrowIfNull(vmbcBytes);

        var model = PdVmVmbcReader.ReadBytes(vmbcBytes);
        PdEdgeHostFunctions.ValidateImports(model.Imports);

        var assemblyName = $"{(string.IsNullOrWhiteSpace(assemblyStem) ? "PdEdge.Program" : assemblyStem)}.{Guid.NewGuid():N}";
        var tempRoot = GetTempWorkRoot();
        Directory.CreateDirectory(tempRoot);
        var outputPath = Path.Combine(tempRoot, $"{assemblyName}.dll");
        PdVmClrCompiler.Compile(
            model,
            outputPath,
            new PdVmCompileOptions
            {
                AssemblyName = assemblyName,
                ModuleName = $"{assemblyName}.dll",
                TypeName = $"PdEdge.Generated.Program_{Guid.NewGuid():N}",
            });

        var assembly = Assembly.LoadFile(outputPath);
        var programType = assembly
            .GetTypes()
            .FirstOrDefault(type =>
                !type.IsAbstract &&
                typeof(IPdVmProgram).IsAssignableFrom(type) &&
                type.GetConstructor(Type.EmptyTypes) is not null);

        if (programType is null)
        {
            throw new InvalidOperationException("no concrete IPdVmProgram implementation found");
        }

        return new PdEdgeLoadedProgram
        {
            Imports = model.Imports,
            UsesAsyncHostOps = PdEdgeHostFunctions.UsesAsyncHostOps(model.Imports),
            CreateProgram = () => (IPdVmProgram)Activator.CreateInstance(programType)!,
        };
    }

    public static async Task CompileSourceFileToVmbcAsync(
        string sourcePath,
        string outputPath,
        CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(sourcePath);
        ArgumentException.ThrowIfNullOrWhiteSpace(outputPath);

        var workspaceRoot = FindWorkspaceRoot();
        Directory.CreateDirectory(Path.GetDirectoryName(Path.GetFullPath(outputPath))!);
        var compilerBinary = FindPrebuiltCompilerBinary(workspaceRoot);
        var normalizedSourcePath = await NormalizeSourcePathAsync(sourcePath, cancellationToken);

        try
        {
            using var process = new Process
            {
                StartInfo = new ProcessStartInfo
                {
                    FileName = compilerBinary ?? "cargo",
                    WorkingDirectory = workspaceRoot,
                    RedirectStandardOutput = true,
                    RedirectStandardError = true,
                    UseShellExecute = false,
                    CreateNoWindow = true,
                },
            };

            if (compilerBinary is null)
            {
                process.StartInfo.ArgumentList.Add("run");
                process.StartInfo.ArgumentList.Add("--quiet");
                process.StartInfo.ArgumentList.Add("--package");
                process.StartInfo.ArgumentList.Add("pd-edge");
                process.StartInfo.ArgumentList.Add("--example");
                process.StartInfo.ArgumentList.Add("compile_to_file");
                process.StartInfo.ArgumentList.Add("--");
            }

            process.StartInfo.ArgumentList.Add(Path.GetFullPath(normalizedSourcePath));
            process.StartInfo.ArgumentList.Add(Path.GetFullPath(outputPath));

            if (!process.Start())
            {
                throw new InvalidOperationException("failed to start cargo for source compilation");
            }

            var stdoutTask = process.StandardOutput.ReadToEndAsync(cancellationToken);
            var stderrTask = process.StandardError.ReadToEndAsync(cancellationToken);
            await process.WaitForExitAsync(cancellationToken);
            var stdout = await stdoutTask;
            var stderr = await stderrTask;
            if (process.ExitCode != 0)
            {
                throw new InvalidOperationException(
                    $"source compilation failed with exit code {process.ExitCode}:{Environment.NewLine}{stdout}{stderr}".Trim());
            }
        }
        finally
        {
            if (!string.Equals(normalizedSourcePath, sourcePath, StringComparison.OrdinalIgnoreCase))
            {
                TryDelete(normalizedSourcePath);
            }
        }
    }

    public static string FindWorkspaceRoot()
    {
        foreach (var seed in EnumerateSearchSeeds())
        {
            var directory = new DirectoryInfo(seed);
            while (directory is not null)
            {
                if (IsWorkspaceRoot(directory.FullName))
                {
                    return directory.FullName;
                }

                directory = directory.Parent;
            }
        }

        throw new InvalidOperationException("failed to locate the project-d workspace root");
    }

    private static IEnumerable<string> EnumerateSearchSeeds()
    {
        yield return Directory.GetCurrentDirectory();
        yield return AppContext.BaseDirectory;
    }

    private static bool IsWorkspaceRoot(string candidate)
    {
        return File.Exists(Path.Combine(candidate, "Cargo.toml")) &&
               Directory.Exists(Path.Combine(candidate, "pd-edge")) &&
               Directory.Exists(Path.Combine(candidate, "pd-vm"));
    }

    private static string? FindPrebuiltCompilerBinary(string workspaceRoot)
    {
        var fileName = OperatingSystem.IsWindows() ? "compile_to_file.exe" : "compile_to_file";
        var debugPath = Path.Combine(workspaceRoot, "target", "debug", "examples", fileName);
        if (File.Exists(debugPath))
        {
            return debugPath;
        }

        var releasePath = Path.Combine(workspaceRoot, "target", "release", "examples", fileName);
        return File.Exists(releasePath) ? releasePath : null;
    }

    private static async Task<string> NormalizeSourcePathAsync(string sourcePath, CancellationToken cancellationToken)
    {
        var bytes = await File.ReadAllBytesAsync(sourcePath, cancellationToken);
        if (bytes.Length < 3 ||
            bytes[0] != 0xEF ||
            bytes[1] != 0xBB ||
            bytes[2] != 0xBF)
        {
            return sourcePath;
        }

        var tempPath = Path.Combine(GetTempWorkRoot(), $"{Guid.NewGuid():N}.rss");
        Directory.CreateDirectory(Path.GetDirectoryName(tempPath)!);
        await File.WriteAllBytesAsync(tempPath, bytes[3..], cancellationToken);
        return tempPath;
    }

    private static string GetTempWorkRoot() =>
        Path.Combine(Path.GetTempPath(), "pd-edge-http-clr");

    private static void TryDelete(string path)
    {
        try
        {
            if (File.Exists(path))
            {
                File.Delete(path);
            }
        }
        catch
        {
        }
    }
}
