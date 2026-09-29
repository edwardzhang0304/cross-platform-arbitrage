// Read-only, bounded diagnostics. Only the Windows system SQLite DLL is loaded.
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Globalization;
using System.Runtime.InteropServices;
using System.Text;
[assembly: DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
public static class CpaDiagnosticsSqlite {
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)] private delegate int Progress(IntPtr unused);
    [DllImport("winsqlite3.dll", CallingConvention=CallingConvention.Cdecl)] private static extern int sqlite3_open_v2(byte[] p,out IntPtr db,int flags,IntPtr vfs);
    [DllImport("winsqlite3.dll", CallingConvention=CallingConvention.Cdecl)] private static extern int sqlite3_db_readonly(IntPtr db,byte[] name);
    [DllImport("winsqlite3.dll", CallingConvention=CallingConvention.Cdecl)] private static extern int sqlite3_busy_timeout(IntPtr db,int ms);
    [DllImport("winsqlite3.dll", CallingConvention=CallingConvention.Cdecl)] private static extern void sqlite3_progress_handler(IntPtr db,int ops,Progress callback,IntPtr arg);
    [DllImport("winsqlite3.dll", CallingConvention=CallingConvention.Cdecl)] private static extern int sqlite3_prepare_v2(IntPtr db,byte[] sql,int size,out IntPtr stmt,IntPtr tail);
    [DllImport("winsqlite3.dll", CallingConvention=CallingConvention.Cdecl)] private static extern int sqlite3_stmt_readonly(IntPtr stmt);
    [DllImport("winsqlite3.dll", CallingConvention=CallingConvention.Cdecl)] private static extern int sqlite3_step(IntPtr stmt);
    [DllImport("winsqlite3.dll", CallingConvention=CallingConvention.Cdecl)] private static extern IntPtr sqlite3_column_text(IntPtr stmt,int col);
    [DllImport("winsqlite3.dll", CallingConvention=CallingConvention.Cdecl)] private static extern int sqlite3_column_bytes(IntPtr stmt,int col);
    [DllImport("winsqlite3.dll", CallingConvention=CallingConvention.Cdecl)] private static extern int sqlite3_finalize(IntPtr stmt);
    [DllImport("winsqlite3.dll", CallingConvention=CallingConvention.Cdecl)] private static extern int sqlite3_close(IntPtr db);
    private static byte[] Utf8(string s) { return Encoding.UTF8.GetBytes(s+"\0"); }
    private static string[] Query(string path,string sql,int limit,int maxBytes) {
        IntPtr db=IntPtr.Zero,stmt=IntPtr.Zero;
        var rows=new List<string>(); var watch=Stopwatch.StartNew();
        Progress progress=delegate(IntPtr unused) { return watch.ElapsedMilliseconds>2000 ? 1 : 0; };
        try {
            int code=sqlite3_open_v2(Utf8(path),out db,1,IntPtr.Zero); // SQLITE_OPEN_READONLY, never CREATE
            if(code!=0) throw new Exception("Readonly SQLite open failed: "+code);
            if(sqlite3_db_readonly(db,Utf8("main"))!=1) throw new Exception("Database is not readonly");
            sqlite3_busy_timeout(db,500); sqlite3_progress_handler(db,1000,progress,IntPtr.Zero);
            code=sqlite3_prepare_v2(db,Utf8(sql),-1,out stmt,IntPtr.Zero);
            if(code!=0) throw new Exception("Readonly query prepare failed: "+code);
            if(sqlite3_stmt_readonly(stmt)!=1) throw new Exception("Query is not readonly");
            int total=0;
            while((code=sqlite3_step(stmt))==100) {
                int size=sqlite3_column_bytes(stmt,0);total+=size;
                if(size<0 || total>maxBytes || rows.Count>=limit) throw new Exception("Readonly result size limit");
                var bytes=new byte[size];if(size>0) Marshal.Copy(sqlite3_column_text(stmt,0),bytes,0,size);
                rows.Add(Encoding.UTF8.GetString(bytes));
            }
            if(code!=101) throw new Exception("Readonly query incomplete or timed out: "+code);
            return rows.ToArray();
        } finally {
            if(stmt!=IntPtr.Zero) sqlite3_finalize(stmt);
            if(db!=IntPtr.Zero) sqlite3_close(db);
            GC.KeepAlive(progress);
        }
    }
    public static string[] State(string path) { return Query(path,"SELECT body FROM state WHERE id=1",1,16*1024*1024); }
    public static string[] Events(string path,long from,long to) {
        string where="kind NOT IN ('sample','halted_market_sample','funding_reconciled')";
        if(from>=0 && to>=from) where+=" AND at_ms BETWEEN "+from.ToString(CultureInfo.InvariantCulture)+" AND "+to.ToString(CultureInfo.InvariantCulture);
        return Query(path,"SELECT CAST(at_ms AS TEXT)||char(9)||kind||char(9)||body FROM events WHERE "+where+" ORDER BY seq DESC LIMIT 120",120,4*1024*1024);
    }
}
