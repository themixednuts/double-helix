//! Pure geometry helpers shared by plugin language hosts.

use crate::requests::UiRect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Fixed(u16),
    Percent(u8),
    Fill,
    Constrained { min: u16, max: u16 },
}

fn allocate(total: u16, sizes: &[Size]) -> Vec<u16> {
    let mut result = vec![0; sizes.len()];
    let mut remaining = total;
    let mut flexible = 0u16;

    for (index, size) in sizes.iter().copied().enumerate() {
        match size {
            Size::Fixed(cells) => {
                let cells = cells.min(remaining);
                result[index] = cells;
                remaining = remaining.saturating_sub(cells);
            }
            Size::Percent(percent) => {
                let cells = ((u32::from(total) * u32::from(percent)) / 100) as u16;
                let cells = cells.min(remaining);
                result[index] = cells;
                remaining = remaining.saturating_sub(cells);
            }
            Size::Fill | Size::Constrained { .. } => flexible = flexible.saturating_add(1),
        }
    }

    if let Some(per_slot) = remaining.checked_div(flexible) {
        let mut extra = remaining % flexible;
        for (index, size) in sizes.iter().copied().enumerate() {
            let bonus = matches!(size, Size::Fill | Size::Constrained { .. }) && extra > 0;
            if bonus {
                extra -= 1;
            }
            let cells = per_slot + u16::from(bonus);
            match size {
                Size::Fill => result[index] = cells,
                Size::Constrained { min, max } => {
                    let max = max.max(1);
                    result[index] = cells.clamp(min.min(max), max);
                }
                Size::Fixed(_) | Size::Percent(_) => {}
            }
        }
    }

    result
}

pub fn split_vertical(area: UiRect, sizes: &[Size]) -> Vec<UiRect> {
    let mut y = area.y;
    allocate(area.height, sizes)
        .into_iter()
        .map(|height| {
            let rect = UiRect {
                x: area.x,
                y,
                width: area.width,
                height,
            };
            y = y.saturating_add(height);
            rect
        })
        .collect()
}

pub fn split_horizontal(area: UiRect, sizes: &[Size]) -> Vec<UiRect> {
    let mut x = area.x;
    allocate(area.width, sizes)
        .into_iter()
        .map(|width| {
            let rect = UiRect {
                x,
                y: area.y,
                width,
                height: area.height,
            };
            x = x.saturating_add(width);
            rect
        })
        .collect()
}

pub fn center(area: UiRect, width: u16, height: u16) -> UiRect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    UiRect {
        x: area.x.saturating_add(area.width.saturating_sub(width) / 2),
        y: area
            .y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> UiRect {
        UiRect {
            x: 10,
            y: 20,
            width: 80,
            height: 24,
        }
    }

    #[test]
    fn allocation_matches_editor_layout_semantics() {
        assert_eq!(
            allocate(
                24,
                &[
                    Size::Fixed(4),
                    Size::Percent(25),
                    Size::Fill,
                    Size::Constrained { min: 2, max: 5 },
                ],
            ),
            vec![4, 6, 7, 5]
        );
        assert_eq!(
            allocate(3, &[Size::Fixed(5), Size::Fixed(2), Size::Fill]),
            vec![3, 0, 0]
        );
    }

    #[test]
    fn split_geometry_preserves_origin_and_axis() {
        let vertical = split_vertical(area(), &[Size::Fixed(4), Size::Fill]);
        assert_eq!(
            vertical[0],
            UiRect {
                height: 4,
                ..area()
            }
        );
        assert_eq!(
            vertical[1],
            UiRect {
                y: 24,
                height: 20,
                ..area()
            }
        );

        let horizontal = split_horizontal(area(), &[Size::Percent(25), Size::Fill]);
        assert_eq!(
            horizontal[0],
            UiRect {
                width: 20,
                ..area()
            }
        );
        assert_eq!(
            horizontal[1],
            UiRect {
                x: 30,
                width: 60,
                ..area()
            }
        );
    }

    #[test]
    fn center_clamps_to_parent() {
        assert_eq!(
            center(area(), 20, 10),
            UiRect {
                x: 40,
                y: 27,
                width: 20,
                height: 10,
            }
        );
        assert_eq!(center(area(), 100, 50), area());
        assert_eq!(
            center(
                UiRect {
                    x: u16::MAX,
                    y: u16::MAX,
                    width: 10,
                    height: 10,
                },
                2,
                2,
            ),
            UiRect {
                x: u16::MAX,
                y: u16::MAX,
                width: 2,
                height: 2,
            }
        );
    }

    #[test]
    fn invalid_constraint_order_is_total() {
        assert_eq!(
            allocate(10, &[Size::Constrained { min: 8, max: 3 }]),
            vec![3]
        );
    }
}
