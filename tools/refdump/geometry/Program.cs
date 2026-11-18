// 几何对拍的真值生成器。
//
// 只编译参考版 C# 源码（SnugWin.Core 的 ScreenFit / CropGeometry / Geometry、
// SnugWin.Windows 的 Win32、SnugWin.App 的 WindowPlacementHelper），不拷贝、不改写。
// 输出三份 TSV，字段见各处注释；Rust 侧 crates/snug-win/tests/geometry_parity.rs 读它们。
//
// 用法：dotnet run --project tools/refdump/geometry -c Release -- tools/geometry-ref
//
// placement 那组会真的创建**一个** WPF 窗口（Opacity=0、不进任务栏、不抢焦点），
// 对它反复调用 WindowPlacementHelper.PlacePhysical 并读回扩展样式与窗口矩形，
// 这是唯一能拿到 C# 真值的方式（那些规则是写在 PlacePhysical 里的，没有独立纯函数）。
// 它只动自己创建的窗口，不碰任何用户窗口。

using System.Globalization;
using System.Text;
using System.Windows;
using System.Windows.Interop;
using SnugWin.App.Views;
using SnugWin.Core.PhoneScreen;
using SnugWin.Windows.Native;

internal static class Program
{
    private static readonly CultureInfo Inv = CultureInfo.InvariantCulture;

    [STAThread]
    private static int Main(string[] args)
    {
        string outDir = args.Length > 0 ? args[0] : "tools/geometry-ref";
        Directory.CreateDirectory(outDir);

        File.WriteAllText(Path.Combine(outDir, "screen_fit.tsv"), DumpScreenFit(), Utf8NoBom());
        File.WriteAllText(Path.Combine(outDir, "crop.tsv"), DumpCrop(), Utf8NoBom());
        File.WriteAllText(Path.Combine(outDir, "placement.tsv"), DumpPlacement(), Utf8NoBom());

        Console.WriteLine($"已写出 {Path.GetFullPath(outDir)}");
        return 0;
    }

    private static Encoding Utf8NoBom() => new UTF8Encoding(false);

    private static string N(double value) => value.ToString("R", Inv);
    private static string I(int value) => value.ToString(Inv);

    // -----------------------------------------------------------------------
    // screen_fit
    // -----------------------------------------------------------------------

    private static string DumpScreenFit()
    {
        var sb = new StringBuilder();
        (int W, int H, int? P)[] phones =
        [
            (1320, 2868, 440),
            (1170, 2532, 390),
            (1080, 2400, 400),
            (1440, 3200, 560),
            (720, 1280, null),
            (1080, 1920, null),
            (1000, 3000, 450),
            (768, 1024, null),
        ];

        foreach (var phone in phones)
        {
            var spec = new PhoneScreenSpecData(phone.W, phone.H, phone.P);
            var fit = ScreenFit.FromPhone(spec);
            string p = phone.P?.ToString(Inv) ?? "";
            sb.Append("FIT\t").Append(I(phone.W)).Append('\t').Append(I(phone.H)).Append('\t')
              .Append(p).Append('\t').Append(N(fit.Ratio)).Append('\t').Append(I(fit.BaseWidth))
              .Append('\t').Append(string.Join(",", fit.Widths.Select(I))).Append('\n');

            foreach (int width in new[] { 540, 600, fit.BaseWidth })
            {
                var size = fit.Size(width);
                sb.Append("SIZE\t").Append(I(phone.W)).Append('\t').Append(I(phone.H)).Append('\t')
                  .Append(p).Append('\t').Append(I(width)).Append('\t')
                  .Append(N(size.Width)).Append('\t').Append(N(size.Height)).Append('\n');

                var (pw, ph) = fit.PixelSize(width);
                sb.Append("PIXEL\t").Append(I(phone.W)).Append('\t').Append(I(phone.H)).Append('\t')
                  .Append(p).Append('\t').Append(I(width)).Append('\t')
                  .Append(I(pw)).Append('\t').Append(I(ph)).Append('\n');
            }
        }

        foreach (double ratio in new[] { 0.5, 1.0, 1.2, 1.5555, 1.759090909090909, 1.7777, 2.0, 2.5 })
        {
            var fit = new ScreenFit(ratio);
            sb.Append("RATIO\t").Append(N(ratio)).Append('\t').Append(N(fit.Ratio)).Append('\t')
              .Append(I(fit.BaseWidth)).Append('\t')
              .Append(string.Join(",", fit.Widths.Select(I))).Append('\n');
        }

        foreach (double screenWidth in new[] { 540.0, 600.0, 720.0, 950.0, 1080.0 })
        {
            sb.Append("BAR\t").Append(N(screenWidth)).Append('\t')
              .Append(N(ScreenFit.Scale(screenWidth))).Append('\t')
              .Append(N(ScreenFit.HomeBarHeight(screenWidth))).Append('\n');
        }

        (double WinW, double WinH, double AreaW, double AreaH, int Current)[] fitTo =
        [
            (800, 900, 540, 950, 540),
            (400, 300, 540, 950, 540),
            (1200, 1000, 540, 950, 540),
            (540, 950, 540, 950, 540),
            (541, 951, 540, 950, 540),
            (700, 500, 600, 900, 600),
            (2000, 1500, 1080, 1800, 1080),
        ];
        foreach (var phone in phones)
        {
            var spec = new PhoneScreenSpecData(phone.W, phone.H, phone.P);
            var fit = ScreenFit.FromPhone(spec);
            string p = phone.P?.ToString(Inv) ?? "";
            foreach (var c in fitTo)
            {
                int result = fit.WidthToFit(
                    new SizeD(c.WinW, c.WinH), new SizeD(c.AreaW, c.AreaH), c.Current);
                sb.Append("FITTO\t").Append(I(phone.W)).Append('\t').Append(I(phone.H)).Append('\t')
                  .Append(p).Append('\t').Append(N(c.WinW)).Append('\t').Append(N(c.WinH)).Append('\t')
                  .Append(N(c.AreaW)).Append('\t').Append(N(c.AreaH)).Append('\t')
                  .Append(I(c.Current)).Append('\t').Append(I(result)).Append('\n');
            }
        }

        return sb.ToString();
    }

    // -----------------------------------------------------------------------
    // crop
    // -----------------------------------------------------------------------

    private static string DumpCrop()
    {
        var sb = new StringBuilder();

        (SizeD Image, RectD Area)[] fitted =
        [
            (new SizeD(1000, 2000), new RectD(0, 0, 540, 900)),
            (new SizeD(1920, 1080), new RectD(10, 20, 540, 900)),
            (new SizeD(1000, 2000), new RectD(100, 50, 800, 400)),
            (new SizeD(0, 100), new RectD(0, 0, 540, 900)),
            (new SizeD(100, 100), new RectD(0, 0, 0, 900)),
        ];
        foreach (var c in fitted)
        {
            var r = CropGeometry.FittedRect(c.Image, c.Area);
            sb.Append("FITTED\t").Append(N(c.Image.Width)).Append('\t').Append(N(c.Image.Height))
              .Append('\t').Append(N(c.Area.X)).Append('\t').Append(N(c.Area.Y))
              .Append('\t').Append(N(c.Area.Width)).Append('\t').Append(N(c.Area.Height))
              .Append('\t').Append(N(r.X)).Append('\t').Append(N(r.Y))
              .Append('\t').Append(N(r.Width)).Append('\t').Append(N(r.Height)).Append('\n');
        }

        (RectD Rect, RectD Shown)[] unit =
        [
            (new RectD(100, 100, 200, 300), new RectD(50, 60, 500, 800)),
            (new RectD(0, 0, 500, 800), new RectD(50, 60, 500, 800)),
            (new RectD(50, 60, 0, 0), new RectD(50, 60, 500, 800)),
            (new RectD(10, 20, 30, 40), new RectD(0, 0, 0, 0)),
        ];
        foreach (var c in unit)
        {
            var u = CropGeometry.UnitRect(c.Rect, c.Shown);
            sb.Append("UNIT\t").Append(N(c.Rect.X)).Append('\t').Append(N(c.Rect.Y))
              .Append('\t').Append(N(c.Rect.Width)).Append('\t').Append(N(c.Rect.Height)).Append('\t')
              .Append(N(c.Shown.X)).Append('\t').Append(N(c.Shown.Y))
              .Append('\t').Append(N(c.Shown.Width)).Append('\t').Append(N(c.Shown.Height))
              .Append('\t').Append(N(u.X)).Append('\t').Append(N(u.Y))
              .Append('\t').Append(N(u.Width)).Append('\t').Append(N(u.Height)).Append('\n');

            var back = CropGeometry.Rect(u, c.Shown);
            sb.Append("RECT\t").Append(N(u.X)).Append('\t').Append(N(u.Y))
              .Append('\t').Append(N(u.Width)).Append('\t').Append(N(u.Height)).Append('\t')
              .Append(N(c.Shown.X)).Append('\t').Append(N(c.Shown.Y))
              .Append('\t').Append(N(c.Shown.Width)).Append('\t').Append(N(c.Shown.Height))
              .Append('\t').Append(N(back.X)).Append('\t').Append(N(back.Y))
              .Append('\t').Append(N(back.Width)).Append('\t').Append(N(back.Height)).Append('\n');
        }

        (RectD Unit, SizeD Image)[] pixels =
        [
            (new RectD(0.1, 0.2, 0.3, 0.4), new SizeD(1320, 2868)),
            (new RectD(-0.1, -0.1, 1.2, 1.2), new SizeD(1000, 2000)),
            (new RectD(0.5, 0.5, 0.0, 0.0), new SizeD(1000, 2000)),
            (new RectD(0.0, 0.0, 1.0, 1.0), new SizeD(0, 2000)),
            (new RectD(0.123456, 0.654321, 0.222222, 0.111111), new SizeD(1170, 2532)),
        ];
        foreach (var c in pixels)
        {
            var r = CropGeometry.PixelRect(c.Unit, c.Image);
            sb.Append("PIXEL\t").Append(N(c.Unit.X)).Append('\t').Append(N(c.Unit.Y))
              .Append('\t').Append(N(c.Unit.Width)).Append('\t').Append(N(c.Unit.Height)).Append('\t')
              .Append(N(c.Image.Width)).Append('\t').Append(N(c.Image.Height)).Append('\t')
              .Append(N(r.X)).Append('\t').Append(N(r.Y)).Append('\t')
              .Append(N(r.Width)).Append('\t').Append(N(r.Height)).Append('\n');
        }

        var rect = new RectD(100, 100, 200, 150);
        (double X, double Y)[] points =
        [
            (100, 100), (300, 100), (100, 250), (300, 250),
            (200, 100), (200, 250), (100, 175), (300, 175),
            (150, 150), (95, 95), (311, 311), (100, 175), (200, 103),
        ];
        foreach (var p in points)
        {
            var handle = CropGeometry.HandleAt(new PointD(p.X, p.Y), rect, 12);
            sb.Append("HANDLE\t").Append(N(p.X)).Append('\t').Append(N(p.Y)).Append('\t')
              .Append(N(rect.X)).Append('\t').Append(N(rect.Y)).Append('\t')
              .Append(N(rect.Width)).Append('\t').Append(N(rect.Height)).Append('\t')
              .Append("12\t").Append(handle is null ? "none" : I((int)handle.Value)).Append('\n');
        }

        var bounds = new RectD(0, 0, 540, 900);
        foreach (CropHandle handle in Enum.GetValues<CropHandle>())
        {
            foreach (var (dx, dy) in new[] { (30.0, 40.0), (-30.0, -40.0), (-500.0, -500.0), (500.0, 500.0), (0.0, 0.0) })
            {
                var r = CropGeometry.Resized(rect, handle, dx, dy, bounds, 20);
                sb.Append("RESIZED\t").Append(N(rect.X)).Append('\t').Append(N(rect.Y))
                  .Append('\t').Append(N(rect.Width)).Append('\t').Append(N(rect.Height)).Append('\t')
                  .Append(I((int)handle)).Append('\t').Append(N(dx)).Append('\t').Append(N(dy))
                  .Append('\t').Append(N(bounds.X)).Append('\t').Append(N(bounds.Y))
                  .Append('\t').Append(N(bounds.Width)).Append('\t').Append(N(bounds.Height)).Append('\t')
                  .Append("20\t").Append(N(r.X)).Append('\t').Append(N(r.Y))
                  .Append('\t').Append(N(r.Width)).Append('\t').Append(N(r.Height)).Append('\n');
            }
        }

        return sb.ToString();
    }

    // -----------------------------------------------------------------------
    // placement
    // -----------------------------------------------------------------------

    private static string DumpPlacement()
    {
        var sb = new StringBuilder();
        const int GWL_EXSTYLE = -20;

        var window = new Window
        {
            WindowStyle = WindowStyle.None,
            ShowInTaskbar = false,
            ShowActivated = false,
            Opacity = 0.0,
            Width = 400,
            Height = 400,
            Left = -2000,
            Top = -2000,
            SizeToContent = SizeToContent.Manual,
            WindowStartupLocation = WindowStartupLocation.Manual,
        };
        var helper = new WindowInteropHelper(window);
        IntPtr hwnd = helper.EnsureHandle();
        window.Show();

        long[] baselines = [0, 0x80, 0x20, 0x100, 0x08000000];
        (double X, double Y, double W, double H)[] boundsCases =
        [
            (10.4, 20.6, 300.5, 200.49),
            (100.5, 50.5, 199.5, 399.5),
            (-500.0, -400.0, 300.0, 200.0),
            (0.0, 0.0, 540.0, 950.0),
        ];

        foreach (long baseline in baselines)
        {
            foreach (bool topmost in new[] { true, false })
            {
                foreach (bool noActivate in new[] { true, false })
                {
                    foreach (bool passthrough in new[] { true, false })
                    {
                        foreach (var bounds in boundsCases)
                        {
                            Win32.SetWindowLongPtrW(hwnd, GWL_EXSTYLE, baseline);
                            WindowPlacementHelper.PlacePhysical(
                                window,
                                new RectD(bounds.X, bounds.Y, bounds.W, bounds.H),
                                topmost,
                                noActivate,
                                focusIfActivated: false,
                                passthrough: passthrough);

                            long desired = Win32.GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                            sb.Append("STYLE\t").Append(baseline.ToString(Inv)).Append('\t')
                              .Append(topmost ? "1" : "0").Append('\t')
                              .Append(noActivate ? "1" : "0").Append('\t')
                              .Append(passthrough ? "1" : "0").Append('\t')
                              .Append(desired.ToString(Inv)).Append('\n');

                            if (Win32.GetWindowRect(hwnd, out Win32.RECT current))
                            {
                                sb.Append("RECT\t").Append(N(bounds.X)).Append('\t').Append(N(bounds.Y))
                                  .Append('\t').Append(N(bounds.W)).Append('\t').Append(N(bounds.H)).Append('\t')
                                  .Append(I(current.Left)).Append('\t').Append(I(current.Top))
                                  .Append('\t').Append(I(current.Width)).Append('\t')
                                  .Append(I(current.Height)).Append('\n');
                            }
                        }
                    }
                }
            }
        }

        window.Close();
        return sb.ToString();
    }
}
