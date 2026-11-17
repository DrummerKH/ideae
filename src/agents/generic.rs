//! Any other program: started as typed, with no sessions of its own the app
//! knows of. What it does is guessed from its terminal (task/activity.rs).

use super::*;

pub struct Generic;

impl Adapter for Generic {
    fn command(&self, profile: &Profile, _: &Start, _: bool, _: Option<&Hooks>) -> String {
        profile.command.trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_typed() {
        let profile = Profile { command: " aider --yes ".into(), kind: Kind::Generic, ..Default::default() };
        assert_eq!(Generic.command(&profile, &Start::Resume("x".into()), true, None), "aider --yes");
        assert!(Generic.one_shot(&profile, &Ask::Quick).is_none());
        assert_eq!(Generic.events(), Events::None);
    }
}
