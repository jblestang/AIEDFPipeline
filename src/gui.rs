use crate::metrics::MetricsSnapshot;
use crossbeam_channel::Receiver;
use eframe::egui;
use egui_plot::{Line, Plot, PlotPoints};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Structure to track statistics over time
#[derive(Clone)]
struct StatisticsPoint {
    time: f64, // Time in seconds since start
    avg: f64,
    min: f64,
    max: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    p100: f64,
    std_dev: f64,
    // Drop metrics
    ingress_drops: u64,
    edf_heap_drops: u64,
    edf_output_drops: u64,
    total_drops: u64,
}

// State to track statistics history per flow
#[derive(Clone)]
struct FlowStatistics {
    start_time: Instant,
    points: VecDeque<StatisticsPoint>, // Use VecDeque for O(1) pop_front
    max_points: usize,
    last_point_time: f64, // Track last point time to avoid duplicates
}

/// Run GUI with direct channel receiver (for internal use)
pub fn run_gui(
    metrics_rx: Receiver<HashMap<u64, MetricsSnapshot>>,
    shutdown_flag: Arc<AtomicBool>,
) {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 1000.0])
            .with_title("AIEDF Pipeline Monitor"),
        ..Default::default()
    };

    // Shared state for statistics history per flow
    let statistics_history = Arc::new(Mutex::new(HashMap::<u64, FlowStatistics>::new()));

    eframe::run_simple_native("AIEDF Pipeline Monitor", options, move |ctx, _frame| {
        update_ui(ctx, &metrics_rx, &statistics_history, &shutdown_flag);
    })
    .unwrap();
}

fn update_ui(
    ctx: &egui::Context,
    metrics_rx: &Receiver<HashMap<u64, MetricsSnapshot>>,
    statistics_history: &Arc<Mutex<HashMap<u64, FlowStatistics>>>,
    _shutdown_flag: &Arc<AtomicBool>,
) {
    // Try to receive latest metrics
    let mut latest_metrics = HashMap::new();

    while let Ok(metrics) = metrics_rx.try_recv() {
        let now = Instant::now();
        let mut stats_guard = statistics_history.lock().unwrap();

        for (flow_id, snapshot) in &metrics {
            let flow_stats = stats_guard
                .entry(*flow_id)
                .or_insert_with(|| FlowStatistics {
                    start_time: now, // Only set start_time when flow is first created
                    points: VecDeque::with_capacity(1000),
                    max_points: 1000, // Keep last 1000 points (reduced to limit memory usage)
                    last_point_time: -1.0, // Initialize to -1 to always add first point
                });

            // Don't reset start_time if flow already exists - use the original start_time
            let elapsed = now.duration_since(flow_stats.start_time).as_secs_f64();

            // Always add a new point - accumulate all statistics over time
            // Convert to milliseconds for plotting (latencies are stored in microseconds internally)
            let point = StatisticsPoint {
                time: elapsed,
                avg: snapshot.avg_latency.as_secs_f64() * 1000.0,
                min: snapshot.min_latency.unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0,
                max: snapshot.max_latency.unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0,
                p50: snapshot.p50.unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0,
                p95: snapshot.p95.unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0,
                p99: snapshot.p99.unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0,
                p100: snapshot.p100.unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0,
                std_dev: snapshot.std_dev.unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0,
                ingress_drops: snapshot.ingress_drops,
                edf_heap_drops: snapshot.edf_heap_drops,
                edf_output_drops: snapshot.edf_output_drops,
                total_drops: snapshot.total_drops,
            };

            // Only add if this is a new point (different time) or if it's the first point
            // This prevents duplicate points at the same timestamp
            if flow_stats.points.is_empty()
                || (flow_stats.points.back().map(|p| p.time).unwrap_or(-1.0) < elapsed - 0.05)
            {
                flow_stats.points.push_back(point);
                flow_stats.last_point_time = elapsed;

                // Keep only the last max_points points (FIFO - pop_front is O(1))
                if flow_stats.points.len() > flow_stats.max_points {
                    flow_stats.points.pop_front();
                }
            }
        }

        drop(stats_guard);
        latest_metrics = metrics;
    }

    // Get statistics history
    let stats_guard = statistics_history.lock().unwrap();
    let stats_history: HashMap<u64, FlowStatistics> =
        stats_guard.iter().map(|(k, v)| (*k, v.clone())).collect();
    drop(stats_guard);

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Pipeline Latency Monitor");

        ui.separator();

        if latest_metrics.is_empty() {
            ui.label("No metrics available yet. Waiting for packets...");
            return;
        }

        ui.horizontal(|ui| {
            ui.label("Flow ID");
            ui.label("Packets");
            ui.label("Avg");
            ui.label("Min");
            ui.label("P50");
            ui.label("P95");
            ui.label("P99");
            ui.label("P100");
            ui.label("StdDev");
            ui.label("Misses");
        });

        ui.separator();

        let mut flows: Vec<_> = latest_metrics.keys().collect();
        flows.sort();

        for &flow_id in &flows {
            let metrics = &latest_metrics[flow_id];
            ui.horizontal(|ui| {
                ui.label(format!("{}", flow_id));
                ui.label(format!("{}", metrics.packet_count));
                // Display with microsecond precision when < 1ms, otherwise milliseconds
                let avg_ms = metrics.avg_latency.as_secs_f64() * 1000.0;
                let avg_str = if avg_ms < 1.0 {
                    format!("{:.3} ms", avg_ms)
                } else {
                    format!("{:.2} ms", avg_ms)
                };
                ui.label(avg_str);

                let format_latency = |d: Duration| {
                    let ms = d.as_secs_f64() * 1000.0;
                    if ms < 1.0 {
                        format!("{:.3} ms", ms)
                    } else {
                        format!("{:.2} ms", ms)
                    }
                };

                ui.label(format_latency(
                    metrics.min_latency.unwrap_or(Duration::ZERO),
                ));
                ui.label(format_latency(metrics.p50.unwrap_or(Duration::ZERO)));
                ui.label(format_latency(metrics.p95.unwrap_or(Duration::ZERO)));
                ui.label(format_latency(metrics.p99.unwrap_or(Duration::ZERO)));
                ui.label(format_latency(metrics.p100.unwrap_or(Duration::ZERO)));
                ui.label(format_latency(metrics.std_dev.unwrap_or(Duration::ZERO)));
                ui.label(format!("{}", metrics.deadline_misses));
            });
        }

        ui.separator();

        // Packet drops display
        ui.heading("Packet Drops");
        ui.horizontal(|ui| {
            ui.label("Flow ID");
            ui.label("Ingress");
            ui.label("EDF Heap");
            ui.label("EDF Output");
            ui.label("Total");
        });
        ui.separator();
        for &flow_id in &flows {
            let metrics = &latest_metrics[flow_id];
            ui.horizontal(|ui| {
                ui.label(format!("{}", flow_id));
                ui.label(format!("{}", metrics.ingress_drops));
                ui.label(format!("{}", metrics.edf_heap_drops));
                ui.label(format!("{}", metrics.edf_output_drops));
                ui.label(format!("{}", metrics.total_drops));
            });
        }

        ui.separator();

        // Real-time statistics plots
        ui.heading("Real-Time Statistics Plots");

        for &flow_id in &flows {
            let metrics = &latest_metrics[flow_id];

            if let Some(flow_stats) = stats_history.get(flow_id) {
                if !flow_stats.points.is_empty() {
                    ui.group(|ui| {
                        ui.label(format!(
                            "Flow {} Statistics Over Time (Expected Max: {:.2} ms)",
                            flow_id,
                            metrics.expected_max_latency.as_secs_f64() * 1000.0
                        ));

                        // Helper function to safely convert to log scale (avoid log(0) or negative values)
                        let to_log_y = |y: f64| {
                            let safe_y = y.max(0.0001); // Minimum 0.0001ms to avoid log(0)
                            safe_y.log10()
                        };

                        // Create plot points for each statistic with log Y scale
                        let avg_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.avg)])
                            .collect();
                        let min_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.min)])
                            .collect();
                        let max_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.max)])
                            .collect();
                        let p50_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.p50)])
                            .collect();
                        let p95_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.p95)])
                            .collect();
                        let p99_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.p99)])
                            .collect();
                        let p100_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.p100)])
                            .collect();
                        let std_dev_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.std_dev)])
                            .collect();

                        // Expected max latency line
                        let expected_max_ms =
                            metrics.max_latency.unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0;
                        let time_range = if let Some(last) = flow_stats.points.back() {
                            last.time.max(1.0) // At least 1 second for initial view
                        } else {
                            1.0
                        };
                        // Calculate Y-axis bounds based on all data points (in linear space)
                        let y_min_linear =
                            flow_stats
                                .points
                                .iter()
                                .map(|p| {
                                    p.min.min(p.avg.min(
                                        p.p50.min(p.p95.min(p.p99.min(p.p100.min(p.std_dev)))),
                                    ))
                                })
                                .fold(f64::INFINITY, f64::min)
                                .min(0.0); // Start at 0 or below
                        let y_max_linear =
                            flow_stats
                                .points
                                .iter()
                                .map(|p| {
                                    p.max.max(p.avg.max(
                                        p.p50.max(p.p95.max(p.p99.max(p.p100.max(p.std_dev)))),
                                    ))
                                })
                                .fold(f64::NEG_INFINITY, f64::max)
                                .max(expected_max_ms * 1.2) // At least 20% above expected max
                                .max(1.0); // At least 1ms

                        // Convert bounds to log scale for plotting
                        let y_min_log = to_log_y(y_min_linear.max(0.0001));
                        let y_max_log = to_log_y(y_max_linear);

                        // Get available width for the plot
                        let plot_width = ui.available_width().max(400.0); // Minimum 400px width

                        Plot::new(format!("flow_{}_stats_plot", flow_id))
                            .width(plot_width)
                            .height(250.0)
                            .x_axis_label("Time (seconds)")
                            .y_axis_label("Latency (ms) - Log Scale")
                            .y_axis_formatter(|val, _range, _| {
                                // Convert from log scale back to linear for display
                                let linear_val = 10f64.powf(val);
                                // Format with appropriate precision for linear value
                                // Note: This formatter is used for both axis labels and hover tooltip
                                if linear_val < 1.0 {
                                    format!("{:.3}", linear_val)
                                } else {
                                    format!("{:.2}", linear_val)
                                }
                            })
                            .x_axis_formatter(|val, _range, _| {
                                format!("{:.1}s", val)
                            })
                            .label_formatter(|name, point| {
                                // Custom formatter for hover tooltip: show real linear value with "ms"
                                let linear_y = 10f64.powf(point.y);
                                let y_str = if linear_y < 1.0 {
                                    format!("{:.3} ms", linear_y)
                                } else {
                                    format!("{:.2} ms", linear_y)
                                };
                                format!("{}\n{:.1}s, {}", name, point.x, y_str)
                            })
                            .include_y(y_min_log)
                            .include_y(y_max_log)
                            .legend(
                                egui_plot::Legend::default().position(egui_plot::Corner::RightTop),
                            )
                            .show(ui, |plot_ui| {
                                // Expected max line in log scale
                                let max_line_points = PlotPoints::new(vec![
                                    [0.0, to_log_y(expected_max_ms)],
                                    [time_range, to_log_y(expected_max_ms)],
                                ]);

                                plot_ui.line(
                                    Line::new(avg_points).name("Avg").color(egui::Color32::BLUE),
                                );
                                plot_ui.line(
                                    Line::new(min_points)
                                        .name("Min")
                                        .color(egui::Color32::GREEN),
                                );
                                plot_ui.line(
                                    Line::new(max_points).name("Max").color(egui::Color32::RED),
                                );
                                plot_ui.line(
                                    Line::new(p50_points)
                                        .name("P50")
                                        .color(egui::Color32::from_rgb(0, 255, 255)),
                                ); // Cyan
                                plot_ui.line(
                                    Line::new(p95_points)
                                        .name("P95")
                                        .color(egui::Color32::YELLOW),
                                );
                                plot_ui.line(
                                    Line::new(p99_points)
                                        .name("P99")
                                        .color(egui::Color32::from_rgb(255, 165, 0)),
                                ); // Orange
                                plot_ui.line(
                                    Line::new(p100_points)
                                        .name("P100")
                                        .color(egui::Color32::from_rgb(255, 0, 255)),
                                ); // Magenta
                                plot_ui.line(
                                    Line::new(std_dev_points)
                                        .name("StdDev")
                                        .color(egui::Color32::from_rgb(128, 128, 128)),
                                ); // Gray
                                plot_ui.line(
                                    Line::new(max_line_points)
                                        .name("Expected Max")
                                        .color(egui::Color32::RED)
                                        .style(egui_plot::LineStyle::Dashed { length: 5.0 }),
                                );
                            });
                    });
                } else {
                    ui.label(format!("Flow {}: No statistics data yet", flow_id));
                }
            } else {
                ui.label(format!("Flow {}: Waiting for statistics...", flow_id));
            }
        }

        ui.separator();

        // Packet drops graph
        ui.heading("Packet Drops Over Time");
        for &flow_id in &flows {
            if let Some(flow_stats) = stats_history.get(&flow_id) {
                if !flow_stats.points.is_empty() {
                    ui.group(|ui| {
                        ui.label(format!("Flow {} Packet Drops", flow_id));

                        // Create plot points for drop metrics
                        let ingress_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, p.ingress_drops as f64])
                            .collect();
                        let edf_heap_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, p.edf_heap_drops as f64])
                            .collect();
                        let edf_output_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, p.edf_output_drops as f64])
                            .collect();
                        let total_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, p.total_drops as f64])
                            .collect();

                        // Calculate Y-axis bounds
                        let y_max = flow_stats
                            .points
                            .iter()
                            .map(|p| {
                                p.ingress_drops
                                    .max(p.edf_heap_drops)
                                    .max(p.edf_output_drops)
                                    .max(p.total_drops) as f64
                            })
                            .fold(0.0, f64::max)
                            .max(1.0); // At least 1 for visibility

                        let time_range = if let Some(last) = flow_stats.points.back() {
                            last.time.max(1.0)
                        } else {
                            1.0
                        };

                        Plot::new(format!("flow_{}_drops_plot", flow_id))
                            .width(ui.available_width())
                            .height(300.0)
                            .x_axis_formatter(|val, _range, _| format!("{:.1}s", val))
                            .label_formatter(|name, point| {
                                format!("{}\n{:.1}s, {:.0} drops", name, point.x, point.y)
                            })
                            .include_x(0.0)
                            .include_x(time_range)
                            .include_y(0.0)
                            .include_y(y_max)
                            .show(ui, |plot_ui| {
                                plot_ui.line(
                                    Line::new(ingress_points)
                                        .name("Ingress Drops")
                                        .color(egui::Color32::RED),
                                );
                                plot_ui.line(
                                    Line::new(edf_heap_points)
                                        .name("EDF Heap Drops")
                                        .color(egui::Color32::from_rgb(255, 165, 0)),
                                );
                                plot_ui.line(
                                    Line::new(edf_output_points)
                                        .name("EDF Output Drops")
                                        .color(egui::Color32::YELLOW),
                                );
                                plot_ui.line(
                                    Line::new(total_points)
                                        .name("Total Drops")
                                        .color(egui::Color32::BLUE),
                                );
                            });
                    });
                } else {
                    ui.label(format!("Flow {}: No drop data yet", flow_id));
                }
            } else {
                ui.label(format!("Flow {}: Waiting for drop data...", flow_id));
            }
        }

        ui.separator();

        // Shutdown button
        if ui.button("Shutdown Pipeline").clicked() {
            _shutdown_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            std::process::exit(0);
        }
    });

    // Request repaint less frequently to reduce flickering (200ms instead of 100ms)
    ctx.request_repaint_after(Duration::from_millis(200));
}

/// Run GUI as a client connecting to TCP metrics server
pub fn run_gui_client(server_addr: &str, shutdown_flag: Arc<AtomicBool>) {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 1000.0])
            .with_title("AIEDF Pipeline Monitor"),
        ..Default::default()
    };

    // Shared state for latest metrics
    let latest_metrics = Arc::new(Mutex::new(HashMap::<u64, MetricsSnapshot>::new()));
    let latest_metrics_clone = latest_metrics.clone();

    // Shared state for statistics history per flow
    let statistics_history = Arc::new(Mutex::new(HashMap::<u64, FlowStatistics>::new()));
    let statistics_history_clone = statistics_history.clone();

    // Spawn thread to connect to metrics server
    let server_addr = server_addr.to_string();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            loop {
                match tokio::net::TcpStream::connect(&server_addr).await {
                    Ok(stream) => {
                        println!("[GUI] Connected to metrics server at {}", server_addr);
                        let mut reader = tokio::io::BufReader::new(stream);
                        use tokio::io::AsyncBufReadExt;

                        let mut line = String::new();
                        loop {
                            line.clear();
                            match reader.read_line(&mut line).await {
                                Ok(0) => {
                                    println!("[GUI] Metrics server closed connection");
                                    break;
                                }
                                Ok(_n) => {
                                    // Parse JSON metrics
                                    match serde_json::from_str::<HashMap<u64, MetricsSnapshot>>(line.trim()) {
                                        Ok(metrics) => {
                                            let now = std::time::Instant::now();
                                            // Set last_update to now for deserialized snapshots
                                            let mut updated_metrics = HashMap::new();
                                            let mut stats_guard = statistics_history_clone.lock().unwrap();

                                            for (k, mut v) in metrics {
                                                v.last_update = now;
                                                updated_metrics.insert(k, v.clone());

                                                // Update statistics history
                                                let flow_stats = stats_guard.entry(k).or_insert_with(|| FlowStatistics {
                                                    start_time: now, // Only set start_time when flow is first created
                                                    points: VecDeque::with_capacity(1000),
                                                    max_points: 10000, // Keep last 10000 points
                                                    last_point_time: -1.0, // Initialize to -1 to always add first point
                                                });

                                                // Don't reset start_time if flow already exists - use the original start_time
                                                let elapsed = now.duration_since(flow_stats.start_time).as_secs_f64();

                                                // Always add a new point - accumulate all statistics over time
                                                // Convert to milliseconds for plotting (latencies are stored in microseconds internally)
                                                let point = StatisticsPoint {
                                                    time: elapsed,
                                                    avg: v.avg_latency.as_secs_f64() * 1000.0,
                                                    min: v.min_latency.unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0,
                                                    max: v.max_latency.unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0,
                                                    p50: v.p50.unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0,
                                                    p95: v.p95.unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0,
                                                    p99: v.p99.unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0,
                                                    p100: v.p100.unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0,
                                                    std_dev: v.std_dev.unwrap_or(Duration::ZERO).as_secs_f64() * 1000.0,
                                                    ingress_drops: v.ingress_drops,
                                                    edf_heap_drops: v.edf_heap_drops,
                                                    edf_output_drops: v.edf_output_drops,
                                                    total_drops: v.total_drops,
                                                };

                                                // Only add if this is a new point (different time) or if it's the first point
                                                // This prevents duplicate points at the same timestamp
                                                if flow_stats.points.is_empty() ||
                                                   (flow_stats.points.back().map(|p| p.time).unwrap_or(-1.0) < elapsed - 0.05) {
                                                    flow_stats.points.push_back(point);
                                                    flow_stats.last_point_time = elapsed;

                                                    // Keep only the last max_points points (FIFO - remove oldest)
                                                    if flow_stats.points.len() > flow_stats.max_points {
                                                        flow_stats.points.pop_front();
                                                    }
                                                }
                                            }

                                            drop(stats_guard);
                                            *latest_metrics_clone.lock().unwrap() = updated_metrics;
                                        }
                                        Err(e) => {
                                            eprintln!("[GUI] Error parsing metrics JSON: {} (line: {})", e, line.trim());
                                            eprintln!("[GUI] This might be due to missing queue occupancy fields. Make sure the pipeline binary is up to date.");
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[GUI] Error reading from metrics server: {}", e);
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[GUI] Failed to connect to metrics server: {}. Retrying in 1 second...", e);
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
        });
    });

    eframe::run_simple_native("AIEDF Pipeline Monitor", options, move |ctx, _frame| {
        update_ui_client(ctx, &latest_metrics, &statistics_history, &shutdown_flag);
    })
    .unwrap();
}

fn update_ui_client(
    ctx: &egui::Context,
    latest_metrics: &Arc<Mutex<HashMap<u64, MetricsSnapshot>>>,
    statistics_history: &Arc<Mutex<HashMap<u64, FlowStatistics>>>,
    _shutdown_flag: &Arc<AtomicBool>,
) {
    // Get latest metrics from shared state
    let metrics_guard = latest_metrics.lock().unwrap();
    let latest_metrics = metrics_guard.clone();
    drop(metrics_guard);

    // Get statistics history
    let stats_guard = statistics_history.lock().unwrap();
    let stats_history: HashMap<u64, FlowStatistics> =
        stats_guard.iter().map(|(k, v)| (*k, v.clone())).collect();
    drop(stats_guard);

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Pipeline Latency Monitor");

        if latest_metrics.is_empty() {
            ui.label("Connecting to metrics server...");
            ui.label("Make sure the pipeline is running (cargo run --release)");
            ui.label("If the pipeline is running, check console for connection errors");
            return;
        }

        ui.separator();

        ui.horizontal(|ui| {
            ui.label("Flow ID");
            ui.label("Packets");
            ui.label("Avg");
            ui.label("Min");
            ui.label("P50");
            ui.label("P95");
            ui.label("P99");
            ui.label("P100");
            ui.label("StdDev");
            ui.label("Misses");
        });

        ui.separator();

        let mut flows: Vec<_> = latest_metrics.keys().collect();
        flows.sort();

        for &flow_id in &flows {
            let metrics = &latest_metrics[flow_id];
            ui.horizontal(|ui| {
                ui.label(format!("{}", flow_id));
                ui.label(format!("{}", metrics.packet_count));
                // Display with microsecond precision when < 1ms, otherwise milliseconds
                let avg_ms = metrics.avg_latency.as_secs_f64() * 1000.0;
                let avg_str = if avg_ms < 1.0 {
                    format!("{:.3} ms", avg_ms)
                } else {
                    format!("{:.2} ms", avg_ms)
                };
                ui.label(avg_str);

                let format_latency = |d: Duration| {
                    let ms = d.as_secs_f64() * 1000.0;
                    if ms < 1.0 {
                        format!("{:.3} ms", ms)
                    } else {
                        format!("{:.2} ms", ms)
                    }
                };

                ui.label(format_latency(
                    metrics.min_latency.unwrap_or(Duration::ZERO),
                ));
                ui.label(format_latency(metrics.p50.unwrap_or(Duration::ZERO)));
                ui.label(format_latency(metrics.p95.unwrap_or(Duration::ZERO)));
                ui.label(format_latency(metrics.p99.unwrap_or(Duration::ZERO)));
                ui.label(format_latency(metrics.p100.unwrap_or(Duration::ZERO)));
                ui.label(format_latency(metrics.std_dev.unwrap_or(Duration::ZERO)));
                ui.label(format!("{}", metrics.deadline_misses));
            });
        }

        ui.separator();

        // Packet drops display
        ui.heading("Packet Drops");
        ui.horizontal(|ui| {
            ui.label("Flow ID");
            ui.label("Ingress");
            ui.label("EDF Heap");
            ui.label("EDF Output");
            ui.label("Total");
        });
        ui.separator();
        for &flow_id in &flows {
            let metrics = &latest_metrics[flow_id];
            ui.horizontal(|ui| {
                ui.label(format!("{}", flow_id));
                ui.label(format!("{}", metrics.ingress_drops));
                ui.label(format!("{}", metrics.edf_heap_drops));
                ui.label(format!("{}", metrics.edf_output_drops));
                ui.label(format!("{}", metrics.total_drops));
            });
        }

        ui.separator();

        // Real-time statistics plots
        ui.heading("Real-Time Statistics Plots");

        for &flow_id in &flows {
            let metrics = &latest_metrics[flow_id];

            if let Some(flow_stats) = stats_history.get(flow_id) {
                if !flow_stats.points.is_empty() {
                    ui.group(|ui| {
                        ui.label(format!(
                            "Flow {} Statistics Over Time (Expected Max: {:.2} ms)",
                            flow_id,
                            metrics.expected_max_latency.as_secs_f64() * 1000.0
                        ));

                        // Helper function to safely convert to log scale (avoid log(0) or negative values)
                        let to_log_y = |y: f64| {
                            let safe_y = y.max(0.0001); // Minimum 0.0001ms to avoid log(0)
                            safe_y.log10()
                        };

                        // Create plot points for each statistic with log Y scale
                        let avg_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.avg)])
                            .collect();
                        let min_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.min)])
                            .collect();
                        let max_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.max)])
                            .collect();
                        let p50_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.p50)])
                            .collect();
                        let p95_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.p95)])
                            .collect();
                        let p99_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.p99)])
                            .collect();
                        let p100_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.p100)])
                            .collect();
                        let std_dev_points: PlotPoints = flow_stats
                            .points
                            .iter()
                            .map(|p| [p.time, to_log_y(p.std_dev)])
                            .collect();

                        // Expected max latency line
                        let expected_max_ms = metrics.expected_max_latency.as_secs_f64() * 1000.0;
                        let time_range = if let Some(last) = flow_stats.points.back() {
                            last.time.max(1.0) // At least 1 second for initial view
                        } else {
                            1.0
                        };
                        // Calculate Y-axis bounds based on all data points (in linear space)
                        let y_min_linear =
                            flow_stats
                                .points
                                .iter()
                                .map(|p| {
                                    p.min.min(p.avg.min(
                                        p.p50.min(p.p95.min(p.p99.min(p.p100.min(p.std_dev)))),
                                    ))
                                })
                                .fold(f64::INFINITY, f64::min)
                                .min(0.0); // Start at 0 or below
                        let y_max_linear = flow_stats
                            .points
                            .iter()
                            .map(|p| p.max.max(p.p100.max(p.std_dev)))
                            .fold(f64::NEG_INFINITY, f64::max)
                            .max(expected_max_ms * 1.2) // At least 20% above expected max
                            .max(1.0); // At least 1ms

                        // Convert bounds to log scale for plotting
                        let y_min_log = to_log_y(y_min_linear.max(0.0001));
                        let y_max_log = to_log_y(y_max_linear);

                        // Get available width for the plot
                        let plot_width = ui.available_width().max(400.0); // Minimum 400px width

                        Plot::new(format!("flow_{}_stats_plot", flow_id))
                            .width(plot_width)
                            .height(250.0)
                            .x_axis_label("Time (seconds)")
                            .y_axis_label("Latency (ms) - Log Scale")
                            .y_axis_formatter(|val, _range, _| {
                                // Convert from log scale back to linear for display
                                let linear_val = 10f64.powf(val);
                                // Format with appropriate precision for linear value
                                // Note: This formatter is used for both axis labels and hover tooltip
                                if linear_val < 1.0 {
                                    format!("{:.3}", linear_val)
                                } else {
                                    format!("{:.2}", linear_val)
                                }
                            })
                            .x_axis_formatter(|val, _range, _| {
                                format!("{:.1}s", val)
                            })
                            .label_formatter(|name, point| {
                                // Custom formatter for hover tooltip: show real linear value with "ms"
                                let linear_y = 10f64.powf(point.y);
                                let y_str = if linear_y < 1.0 {
                                    format!("{:.3} ms", linear_y)
                                } else {
                                    format!("{:.2} ms", linear_y)
                                };
                                format!("{}\n{:.1}s, {}", name, point.x, y_str)
                            })
                            .include_y(y_min_log)
                            .include_y(y_max_log)
                            .legend(
                                egui_plot::Legend::default().position(egui_plot::Corner::RightTop),
                            )
                            .show(ui, |plot_ui| {
                                // Expected max line in log scale
                                let max_line_points = PlotPoints::new(vec![
                                    [0.0, to_log_y(expected_max_ms)],
                                    [time_range, to_log_y(expected_max_ms)],
                                ]);

                                plot_ui.line(
                                    Line::new(avg_points).name("Avg").color(egui::Color32::BLUE),
                                );
                                plot_ui.line(
                                    Line::new(min_points)
                                        .name("Min")
                                        .color(egui::Color32::GREEN),
                                );
                                plot_ui.line(
                                    Line::new(max_points).name("Max").color(egui::Color32::RED),
                                );
                                plot_ui.line(
                                    Line::new(p50_points)
                                        .name("P50")
                                        .color(egui::Color32::from_rgb(0, 255, 255)),
                                ); // Cyan
                                plot_ui.line(
                                    Line::new(p95_points)
                                        .name("P95")
                                        .color(egui::Color32::YELLOW),
                                );
                                plot_ui.line(
                                    Line::new(p99_points)
                                        .name("P99")
                                        .color(egui::Color32::from_rgb(255, 165, 0)),
                                ); // Orange
                                plot_ui.line(
                                    Line::new(p100_points)
                                        .name("P100")
                                        .color(egui::Color32::from_rgb(255, 0, 255)),
                                ); // Magenta
                                plot_ui.line(
                                    Line::new(std_dev_points)
                                        .name("StdDev")
                                        .color(egui::Color32::from_rgb(128, 128, 128)),
                                ); // Gray
                                plot_ui.line(
                                    Line::new(max_line_points)
                                        .name("Expected Max")
                                        .color(egui::Color32::RED)
                                        .style(egui_plot::LineStyle::Dashed { length: 5.0 }),
                                );
                            });
                    });
                } else {
                    ui.label(format!("Flow {}: No statistics data yet", flow_id));
                }
            } else {
                ui.label(format!("Flow {}: Waiting for statistics...", flow_id));
            }
        }

        ui.separator();

        // Close button (GUI doesn't control pipeline shutdown anymore)
        if ui.button("Close GUI").clicked() {
            std::process::exit(0);
        }
    });

    // Request repaint less frequently to reduce flickering (200ms instead of 100ms)
    ctx.request_repaint_after(Duration::from_millis(200));
}
