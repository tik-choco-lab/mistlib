using System;
using System.Runtime.CompilerServices;
using UnityEngine;

namespace MistLib
{
    public static class MistLogger
    {
        public static LogLevel MinLogLevel = LogLevel.Info;
        public static bool UseJsonFormat = false;

        public enum LogLevel
        {
            Debug = 0,
            Info = 1,
            Warning = 2,
            Error = 3,
            Fatal = 4,
            None = 5
        }

        [Serializable]
        private class LogFormat
        {
            public string timestamp;
            public string level;
            public string message;
            public string caller;

            public void Set(string timestamp, string level, string caller, string message)
            {
                this.timestamp = timestamp;
                this.level = level;
                this.caller = caller;
                this.message = message;
            }
        }

        private static readonly LogFormat LogData = new();

        private static string Format(LogLevel level, object message, string filePath, int lineNumber)
        {
            return UseJsonFormat
                ? FormatJson(level, message, filePath, lineNumber)
                : FormatString(level, message, filePath, lineNumber);
        }

        private static string FormatString(LogLevel level, object message, string filePath, int lineNumber)
        {
            return $"{GetLevelText(level)} {message} {ShortenPath(filePath)}:{lineNumber}";
        }

        private static string FormatJson(LogLevel level, object message, string filePath, int lineNumber)
        {
            LogData.Set(
                DateTime.Now.ToString("o"),
                GetLevelText(level, true),
                ShortenPath(filePath) + ":" + lineNumber,
                message?.ToString()
            );
            return JsonUtility.ToJson(LogData);
        }

        private static string GetLevelText(LogLevel level, bool plain = false)
        {
            if (plain) return level.ToString().ToUpper();

            return level switch
            {
                LogLevel.Debug => "<color=#00ff00>DEBUG</color>",
                LogLevel.Info => "<color=#00ffff>INFO</color>",
                LogLevel.Warning => "<color=#ffff00>WARNING</color>",
                LogLevel.Error => "<color=#ffb6b7>ERROR</color>",
                LogLevel.Fatal => "<color=#ff64ff>FATAL</color>",
                _ => ""
            };
        }

        private static string ShortenPath(string path)
        {
            if (string.IsNullOrEmpty(path)) return "Unknown";
            var parts = path.Replace("\\", "/").Split('/');
            return parts.Length >= 2 ? $"{parts[^2]}/{parts[^1]}" : parts[^1];
        }

        public static void Log(object message, LogLevel level = LogLevel.Info, [CallerFilePath] string file = "", [CallerLineNumber] int line = 0)
        {
            if (level < MinLogLevel) return;

            var msg = Format(level, message, file, line);
            switch (level)
            {
                case LogLevel.Warning: UnityEngine.Debug.LogWarning(msg); break;
                case LogLevel.Error:
                case LogLevel.Fatal: UnityEngine.Debug.LogError(msg); break;
                default: UnityEngine.Debug.Log(msg); break;
            }
        }

        public static void Debug(object message, [CallerFilePath] string file = "", [CallerLineNumber] int line = 0) => Log(message, LogLevel.Debug, file, line);
        public static void Info(object message, [CallerFilePath] string file = "", [CallerLineNumber] int line = 0) => Log(message, LogLevel.Info, file, line);
        public static void Warning(object message, [CallerFilePath] string file = "", [CallerLineNumber] int line = 0) => Log(message, LogLevel.Warning, file, line);
        public static void Error(object message, [CallerFilePath] string file = "", [CallerLineNumber] int line = 0) => Log(message, LogLevel.Error, file, line);
        public static void Fatal(object message, [CallerFilePath] string file = "", [CallerLineNumber] int line = 0) => Log(message, LogLevel.Fatal, file, line);
    }
}
