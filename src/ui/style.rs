use console::{style, StyledObject};

// 错误输出样式
pub fn estyle<D>(val: D) -> StyledObject<D> {
    style(val).for_stderr()
}

pub fn ecyan<D>(val: D) -> StyledObject<D> {
    estyle(val).cyan()
}
pub fn eblue<D>(val: D) -> StyledObject<D> {
    estyle(val).blue()
}
pub fn emagenta<D>(val: D) -> StyledObject<D> {
    estyle(val).magenta()
}
pub fn egreen<D>(val: D) -> StyledObject<D> {
    estyle(val).green()
}
pub fn eyellow<D>(val: D) -> StyledObject<D> {
    estyle(val).yellow()
}
pub fn ered<D>(val: D) -> StyledObject<D> {
    estyle(val).red()
}
pub fn eblack<D>(val: D) -> StyledObject<D> {
    estyle(val).black()
}
pub fn eunderline<D>(val: D) -> StyledObject<D> {
    estyle(val).underlined()
}

pub fn edim<D>(val: D) -> StyledObject<D> {
    estyle(val).dim()
}

pub fn ebold<D>(val: D) -> StyledObject<D> {
    estyle(val).bold()
}

// 普通输出样式
pub fn ostyle<D>(val: D) -> StyledObject<D> {
    style(val).for_stdout()
}

pub fn ocyan<D>(val: D) -> StyledObject<D> {
    ostyle(val).cyan()
}
pub fn oblue<D>(val: D) -> StyledObject<D> {
    ostyle(val).blue()
}
pub fn omagenta<D>(val: D) -> StyledObject<D> {
    ostyle(val).magenta()
}
pub fn ogreen<D>(val: D) -> StyledObject<D> {
    ostyle(val).green()
}
pub fn oyellow<D>(val: D) -> StyledObject<D> {
    ostyle(val).yellow()
}
pub fn ored<D>(val: D) -> StyledObject<D> {
    ostyle(val).red()
}
pub fn oblack<D>(val: D) -> StyledObject<D> {
    ostyle(val).black()
}
pub fn ounderline<D>(val: D) -> StyledObject<D> {
    ostyle(val).underlined()
}

pub fn odim<D>(val: D) -> StyledObject<D> {
    ostyle(val).dim()
}

pub fn obold<D>(val: D) -> StyledObject<D> {
    ostyle(val).bold()
}
