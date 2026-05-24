use super::{LaunchConfigInput, TargetConfigInput};
use anyhow::Context;
use std::collections::BTreeMap;

pub(super) struct TemplateChainResolver<'a, Input> {
    pub(super) kind: &'static str,
    pub(super) templates: &'a BTreeMap<String, Input>,
    pub(super) dependencies: fn(&Input) -> &[String],
    pub(super) overlay: fn(Input, &Input) -> anyhow::Result<Input>,
}

impl<Input> TemplateChainResolver<'_, Input> {
    pub(super) fn validate_references(&self) -> anyhow::Result<()> {
        for name in self.templates.keys() {
            self.check_template_references(name, &mut Vec::new())?;
        }
        Ok(())
    }

    pub(super) fn overlay_templates(
        &self,
        mut base: Input,
        owner: &str,
        templates: &[String],
    ) -> anyhow::Result<Input> {
        for template in templates {
            base = self
                .apply_template(base, template, &mut Vec::new())
                .with_context(|| format!("{owner} uses {} template {template:?}", self.kind))?;
        }
        Ok(base)
    }

    fn check_template_references(&self, name: &str, stack: &mut Vec<String>) -> anyhow::Result<()> {
        let template = self.template_or_cycle_error(name, stack)?;

        stack.push(name.to_string());
        for dependency in (self.dependencies)(template) {
            self.check_template_references(dependency, stack)
                .with_context(|| self.dependency_context(name, dependency))?;
        }
        stack.pop();
        Ok(())
    }

    fn apply_template(
        &self,
        mut base: Input,
        name: &str,
        stack: &mut Vec<String>,
    ) -> anyhow::Result<Input> {
        let template = self.template_or_cycle_error(name, stack)?;

        stack.push(name.to_string());
        for dependency in (self.dependencies)(template) {
            base = self
                .apply_template(base, dependency, stack)
                .with_context(|| self.dependency_context(name, dependency))?;
        }
        stack.pop();
        (self.overlay)(base, template)
    }

    fn template_or_cycle_error(&self, name: &str, stack: &[String]) -> anyhow::Result<&Input> {
        if let Some(start) = stack.iter().position(|entry| entry == name) {
            let mut cycle = stack[start..].to_vec();
            cycle.push(name.to_string());
            anyhow::bail!("{} template cycle: {}", self.kind, cycle.join(" -> "));
        }
        self.templates
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown {} template {name:?}", self.kind))
    }

    fn dependency_context(&self, name: &str, dependency: &str) -> String {
        format!(
            "{} template {name:?} uses {} template {dependency:?}",
            self.kind, self.kind
        )
    }
}

pub(super) fn target_template_dependencies(template: &TargetConfigInput) -> &[String] {
    &template.use_templates
}

pub(super) fn launch_template_dependencies(template: &LaunchConfigInput) -> &[String] {
    &template.use_templates
}

pub(super) fn overlay_target_template(
    base: TargetConfigInput,
    template: &TargetConfigInput,
) -> anyhow::Result<TargetConfigInput> {
    base.overlay(template)
}

pub(super) fn overlay_launch_template(
    base: LaunchConfigInput,
    template: &LaunchConfigInput,
) -> anyhow::Result<LaunchConfigInput> {
    base.overlay(template)
}
