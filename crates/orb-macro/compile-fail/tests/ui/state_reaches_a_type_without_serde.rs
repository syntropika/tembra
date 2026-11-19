#[tembra::orb(package = "example:plotter@1.0.0")]
mod plotter {
    pub mod drawing {
        use super::Point;

        #[state]
        #[derive(Default, serde::Serialize, serde::Deserialize)]
        pub struct Drawn {
            pub last: Option<Point>,
        }

        #[service]
        pub struct Drawing;

        impl Drawing {
            pub fn draw(at: Point) -> Point {
                Drawn::update(|drawn| drawn.last = Some(at));
                at
            }
        }
    }

    #[derive(Clone, Copy, serde::Deserialize)]
    pub struct Point {
        pub x: i32,
        pub y: i32,
    }
}

fn main() {}
