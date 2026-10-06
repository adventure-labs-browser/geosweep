//! Geographic helpers: fibonacci-sphere seed points and quad-tree cell
//! subdivision — a direct port of the original Python implementation.

const KM_PER_LAT_DEG: f64 = 110.574;
const KM_PER_LON_DEG_EQUATOR: f64 = 111.320;

fn r6(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}

/// Roughly evenly distributed `(lat, lon)` points on a sphere.
pub fn fibonacci_sphere(n: usize) -> Vec<(f64, f64)> {
    let golden = std::f64::consts::PI * (3.0 - 5f64.sqrt());
    (0..n)
        .map(|i| {
            let y = 1.0 - (i as f64 + 0.5) * 2.0 / n as f64;
            let r = (1.0 - y * y).max(0.0).sqrt();
            let theta = golden * i as f64;
            let lat = y.asin().to_degrees();
            let lon = (r * theta.sin()).atan2(r * theta.cos()).to_degrees();
            let lon = (lon + 180.0).rem_euclid(360.0) - 180.0;
            (r6(lat), r6(lon))
        })
        .collect()
}

/// A search cell: center + radius, with a deterministic string id.
#[derive(Debug, Clone)]
pub struct Cell {
    pub id: String,
    pub lat: f64,
    pub lon: f64,
    pub radius: f64,
}

impl Cell {
    pub fn new(lat: f64, lon: f64, radius: f64) -> Self {
        let lat = r6(lat.clamp(-90.0, 90.0));
        let lon = r6((lon + 180.0).rem_euclid(360.0) - 180.0);
        let radius = (radius * 10.0).round() / 10.0;
        Self {
            id: cell_id(lat, lon, radius),
            lat,
            lon,
            radius,
        }
    }
}

pub fn cell_id(lat: f64, lon: f64, radius: f64) -> String {
    format!("{lat:.6}_{lon:.6}_{radius:.1}")
}

/// Split a cell into 4 children at half the radius.
pub fn subdivide(c: &Cell) -> Vec<Cell> {
    let half_km = (c.radius / 2.0) / 1000.0;
    let half_lat = half_km / KM_PER_LAT_DEG;
    // Clamp cosine at ~85deg to keep the longitude step sane near poles.
    let cos_lat = c.lat.to_radians().cos().max(0.0872);
    let half_lon = half_km / (KM_PER_LON_DEG_EQUATOR * cos_lat);
    let r = c.radius / 2.0;
    vec![
        Cell::new(c.lat - half_lat, c.lon - half_lon, r),
        Cell::new(c.lat - half_lat, c.lon + half_lon, r),
        Cell::new(c.lat + half_lat, c.lon - half_lon, r),
        Cell::new(c.lat + half_lat, c.lon + half_lon, r),
    ]
}
